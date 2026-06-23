//! Narration: walk a document's blocks and turn each into spoken [`Sentence`]s,
//! either by streaming an explanation from local Ollama (speech starts before a
//! block finishes) or, offline, via the deterministic humanizer. Ported from the
//! TS `narrator.ts` + `prompts.ts`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::humanize::{humanize_code, humanize_prose};
use crate::ollama::{is_available, stream_chat, OllamaConfig, OllamaErrorCode};
use crate::sentence::{split_sentences, SentenceStreamer};
use crate::types::{Block, BlockKind, Sentence};

/// Where the narrator pushes finished sentences: it appends to the shared list
/// the synth worker reads by index (enabling random-access seek), and reports
/// cancellation so narration stops promptly on quit.
pub struct Emitter {
    sentences: Arc<Mutex<Vec<Sentence>>>,
    cancel: Arc<AtomicBool>,
}

impl Emitter {
    pub fn new(sentences: Arc<Mutex<Vec<Sentence>>>, cancel: Arc<AtomicBool>) -> Self {
        Self { sentences, cancel }
    }

    /// Append a sentence, tagged with the source line range of the `block` it
    /// came from so the TUI can map it back to the original document. Returns
    /// false if narration should stop (cancelled).
    pub fn emit(&self, text: String, block: &Block) -> bool {
        if self.cancel.load(Relaxed) {
            return false;
        }
        self.sentences.lock().unwrap().push(Sentence {
            text,
            start_line: block.start_line,
            end_line: block.end_line,
        });
        true
    }
}

#[derive(Clone, Copy)]
pub enum TableMode {
    /// Offline, "summarize" reads the cells (true summarization needs the model).
    Summarize,
    Read,
    Skip,
}

#[derive(Clone, Copy)]
pub enum CodeMode {
    /// Offline, "explain" falls back to humanizing the code line by line.
    Explain,
    Literal,
    Skip,
}

#[derive(Clone, Copy)]
pub struct NarrationSettings {
    pub announce_headings: bool,
    pub tables: TableMode,
    pub code: CodeMode,
}

impl Default for NarrationSettings {
    fn default() -> Self {
        Self {
            announce_headings: true,
            tables: TableMode::Summarize,
            code: CodeMode::Explain,
        }
    }
}

// ---------------------------------------------------------------------------
// Offline narration (M1) — also the streaming fallback
// ---------------------------------------------------------------------------

/// The deterministic spoken text for one block, or `None` if it is skipped.
fn humanize_block_text(
    block: &Block,
    settings: &NarrationSettings,
    dict: &HashMap<String, String>,
) -> Option<String> {
    match block.kind {
        BlockKind::Heading => {
            let head = humanize_prose(&block.source, dict);
            Some(if settings.announce_headings {
                format!("Heading. {head}")
            } else {
                head
            })
        }
        BlockKind::Table => {
            if matches!(settings.tables, TableMode::Skip) {
                None
            } else {
                Some(humanize_prose(&table_to_speech(&block.source), dict))
            }
        }
        BlockKind::Code => {
            if matches!(settings.code, CodeMode::Skip) {
                None
            } else {
                Some(humanize_code(&block.source, &block.lang, dict))
            }
        }
        // para / quote / listItem / comment
        _ => Some(humanize_prose(&block.source, dict)),
    }
}

/// Narrate all blocks offline into a flat sentence stream (no Ollama).
pub fn narrate_offline(
    blocks: &[Block],
    locale: &str,
    settings: &NarrationSettings,
) -> Vec<Sentence> {
    let dict: HashMap<String, String> = HashMap::new();
    let mut out: Vec<Sentence> = Vec::new();

    for block in blocks {
        let Some(text) = humanize_block_text(block, settings, &dict) else {
            continue;
        };
        for s in split_sentences(&text, locale) {
            if s.trim().is_empty() {
                continue;
            }
            out.push(Sentence {
                text: s,
                start_line: block.start_line,
                end_line: block.end_line,
            });
        }
    }
    out
}

/// Offline fallback for tables: read each row's cells, dropping the `|---|` rule.
fn table_to_speech(source: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for raw in source.split('\n') {
        let row = raw.trim_end_matches('\r').trim();
        if row.is_empty() {
            continue;
        }
        if row.contains('-')
            && row
                .chars()
                .all(|c| c == ' ' || c == '\t' || c == '|' || c == ':' || c == '-')
        {
            continue;
        }
        let mut s = row;
        if let Some(r) = s.strip_prefix('|') {
            s = r;
        }
        if let Some(r) = s.strip_suffix('|') {
            s = r;
        }
        let cells: Vec<String> = s
            .split('|')
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect();
        if !cells.is_empty() {
            out.push(cells.join(", "));
        }
    }
    out.join(". ")
}

// ---------------------------------------------------------------------------
// Streaming narration (M2)
// ---------------------------------------------------------------------------

/// Walk blocks, streaming each through Ollama (or humanizing it offline) and
/// appending complete sentences via `emit` as they form. Stops early if `emit`
/// reports cancellation (teardown).
pub async fn stream_narration(
    blocks: &[Block],
    settings: &NarrationSettings,
    cfg: &OllamaConfig,
    locale: &str,
    emit: &Emitter,
) {
    let dict: HashMap<String, String> = HashMap::new();
    let mut use_ai = is_available(&cfg.base_url, Duration::from_millis(2000)).await;
    if use_ai {
        crate::diag!("[narrate] explaining with {} via {}", cfg.model, cfg.base_url);
    } else {
        crate::diag!(
            "[narrate] Ollama unreachable at {} — using the offline humanizer.",
            cfg.base_url
        );
    }

    for block in blocks {
        // Headings are tiny — humanize them directly rather than spend a call.
        if block.kind == BlockKind::Heading {
            if !send_humanized(block, settings, locale, &dict, emit) {
                return;
            }
            continue;
        }

        // Skips.
        if matches!(block.kind, BlockKind::Table) && matches!(settings.tables, TableMode::Skip) {
            continue;
        }
        if matches!(block.kind, BlockKind::Code) && matches!(settings.code, CodeMode::Skip) {
            continue;
        }

        let force_humanize = match block.kind {
            BlockKind::Table => matches!(settings.tables, TableMode::Read) || !use_ai,
            BlockKind::Code => matches!(settings.code, CodeMode::Literal) || !use_ai,
            _ => !use_ai,
        };
        if force_humanize {
            if !send_humanized(block, settings, locale, &dict, emit) {
                return;
            }
            continue;
        }

        let kind = match block.kind {
            BlockKind::Code => PromptKind::Code,
            BlockKind::Table => PromptKind::Table,
            _ => PromptKind::Prose,
        };
        match stream_block(cfg, block, kind, locale, emit).await {
            StreamOutcome::Stop => return,
            StreamOutcome::Ok => {}
            StreamOutcome::Fallback { emitted } => {
                use_ai = false;
                // Don't re-narrate a block that already streamed partial output,
                // or the listener hears the first half twice.
                if emitted == 0 && !send_humanized(block, settings, locale, &dict, emit) {
                    return;
                }
            }
        }
    }
}

enum StreamOutcome {
    Ok,
    Fallback { emitted: usize },
    Stop,
}

/// Stream one block through Ollama, emitting sentences as they complete.
async fn stream_block(
    cfg: &OllamaConfig,
    block: &Block,
    kind: PromptKind,
    locale: &str,
    emit: &Emitter,
) -> StreamOutcome {
    let mut streamer = SentenceStreamer::new(locale);
    let mut emitted = 0usize;
    let mut stopped = false;

    let result = stream_chat(
        cfg,
        &system_prompt(block, kind),
        &user_prompt(block),
        |delta| {
            if stopped {
                return;
            }
            for s in streamer.push(delta) {
                if s.trim().is_empty() {
                    continue;
                }
                if !emit.emit(s, block) {
                    stopped = true;
                    return;
                }
                emitted += 1;
            }
        },
    )
    .await;

    if stopped {
        return StreamOutcome::Stop;
    }
    match result {
        Ok(()) => {
            for s in streamer.flush() {
                if s.trim().is_empty() {
                    continue;
                }
                if !emit.emit(s, block) {
                    return StreamOutcome::Stop;
                }
                emitted += 1;
            }
            StreamOutcome::Ok
        }
        Err(e) if e.code == OllamaErrorCode::Aborted => StreamOutcome::Stop,
        Err(e) => {
            crate::diag!(
                "[narrate] block at line {}: {} — offline humanizer for the rest.",
                block.start_line, e
            );
            StreamOutcome::Fallback { emitted }
        }
    }
}

/// Humanize a block offline and push its sentences. Returns false if the
/// receiver is gone.
fn send_humanized(
    block: &Block,
    settings: &NarrationSettings,
    locale: &str,
    dict: &HashMap<String, String>,
    emit: &Emitter,
) -> bool {
    if let Some(text) = humanize_block_text(block, settings, dict) {
        for s in split_sentences(&text, locale) {
            if s.trim().is_empty() {
                continue;
            }
            if !emit.emit(s, block) {
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Prompts (ported from prompts.ts) — output goes straight to TTS, so it must be
// plain spoken prose: no markdown, no symbols spelled out as words.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum PromptKind {
    Code,
    Prose,
    Table,
}

const SPEAK_RULES: &str = "Write only plain spoken prose in complete sentences. No markdown, no headings, no bullet points, no code fences, no emoji. Never spell out punctuation or symbols (do not say \"underscore\", \"open brace\", \"hash\", or \"pipe\"). Pronounce identifiers as ordinary words: read snake_case and camelCase as separate words and expand obvious abbreviations (for example, \"fn\" is \"function\", \"idx\" is \"index\"). Keep it concise and easy to follow by ear. Output ONLY the narration itself — no preamble, no sign-off, no meta-commentary such as \"here is the rewritten text\".";

const CODE_EXAMPLE: &str = "Example. Given this code:\nfunction hello(name: string): string {\n  if (name) {\n    return \"Hello, #{name}\"\n  }\n  return \"Hello, World!\"\n}\na good narration is: \"Hello function accepts a single parameter, name, which is expected to be a string. If name is not empty, it returns a personalized greeting. Otherwise, it returns Hello, World.\" Notice it explains the parameter and its type, describes each branch, and does NOT read the string-interpolation token literally.";

const TABLE_EXAMPLE: &str = "Example. A table comparing two systems across many attributes (order source, order type, inventory model, and so on) should be distilled to its essence, for instance: \"PMI is a traditional SAP customer-order business. PACT is a Shopify resale business with item master data available but unused for returns.\" Do not read the columns or rows.";

fn system_prompt(block: &Block, kind: PromptKind) -> String {
    match kind {
        PromptKind::Code => {
            let lang = if block.lang.is_empty() { "code" } else { &block.lang };
            format!(
                "You are a narrator who explains source code out loud to a developer who cannot see the screen. \
Explain what this {lang} does: its purpose, the parameters it takes and their types, what it returns, and any notable conditions or side effects. \
Do NOT read the code line by line or quote it verbatim, and do NOT read string-interpolation or format placeholders (like #{{...}}, ${{...}}, %s) literally — describe the resulting value instead. \
Speak in a few short sentences. {SPEAK_RULES}\n\n{CODE_EXAMPLE}"
            )
        }
        PromptKind::Table => format!(
            "You are a narrator who explains documentation out loud. The following is a table. \
Do NOT read the rows or cells one by one. Summarize what the table conveys in one or two short sentences — the key comparison or takeaway a listener needs. \
{SPEAK_RULES}\n\n{TABLE_EXAMPLE}"
        ),
        PromptKind::Prose => format!(
            "You are a narrator who reads technical documentation out loud naturally. \
Rewrite the following text so it sounds good when spoken: drop any markup, expand identifiers and abbreviations, and smooth out anything awkward to hear. \
Preserve the meaning and the important details — do not drop information. {SPEAK_RULES}"
        ),
    }
}

fn user_prompt(block: &Block) -> String {
    let label = match block.kind {
        BlockKind::Code => format!("Code ({}):", block.lang),
        BlockKind::Comment => "Code comment:".to_string(),
        BlockKind::Table => "Table:".to_string(),
        _ => "Text:".to_string(),
    };
    format!("{label}\n\n{}", block.source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::segment_blocks;

    #[test]
    fn narrates_markdown_offline() {
        let md = "# Setup\n\nRun `npm install` first.\n\n- step one\n- step two";
        let blocks = segment_blocks(md, "markdown", 1);
        let s = narrate_offline(&blocks, "en", &NarrationSettings::default());
        assert!(!s.is_empty());
        assert!(s[0].text.starts_with("Heading."), "{:?}", s[0].text);
        assert!(s.iter().any(|x| x.text.to_lowercase().contains("install")));
    }

    #[test]
    fn table_cells_read() {
        let t = table_to_speech("| A | B |\n| --- | --- |\n| 1 | 2 |");
        assert_eq!(t, "A, B. 1, 2");
    }

    #[test]
    fn prompts_are_plain() {
        let block = Block {
            kind: BlockKind::Code,
            source: "fn x() {}".into(),
            start_line: 1,
            end_line: 1,
            heading_level: None,
            lang: "rust".into(),
        };
        let sys = system_prompt(&block, PromptKind::Code);
        assert!(sys.contains("rust"));
        assert!(sys.contains("explains source code"));
        let user = user_prompt(&block);
        assert!(user.starts_with("Code (rust):"));
    }
}
