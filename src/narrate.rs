//! Offline narration: walk a document's blocks and turn each into spoken
//! [`Sentence`]s using the deterministic humanizer. This is the M1 walking
//! skeleton (no Ollama); the AI-streaming path is added in M2, falling back to
//! exactly this logic when Ollama is unavailable. Ported from the offline
//! branches of the TS `narrator.ts`.

use std::collections::HashMap;

use crate::humanize::{humanize_code, humanize_prose};
use crate::sentence::split_sentences;
use crate::types::{Block, BlockKind, Sentence};

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

/// Narrate all blocks offline into a flat sentence stream.
pub fn narrate_offline(blocks: &[Block], locale: &str, settings: &NarrationSettings) -> Vec<Sentence> {
    let dict: HashMap<String, String> = HashMap::new();
    let mut out: Vec<Sentence> = Vec::new();
    let mut idx = 0usize;

    for (bi, block) in blocks.iter().enumerate() {
        match block.kind {
            BlockKind::Heading => {
                let head = humanize_prose(&block.source, &dict);
                let text = if settings.announce_headings {
                    format!("Heading. {head}")
                } else {
                    head
                };
                push_sentences(&mut out, &mut idx, &text, block, bi, locale);
            }
            BlockKind::Table => {
                if matches!(settings.tables, TableMode::Skip) {
                    continue;
                }
                let text = humanize_prose(&table_to_speech(&block.source), &dict);
                push_sentences(&mut out, &mut idx, &text, block, bi, locale);
            }
            BlockKind::Code => {
                if matches!(settings.code, CodeMode::Skip) {
                    continue;
                }
                let text = humanize_code(&block.source, &block.lang, &dict);
                push_sentences(&mut out, &mut idx, &text, block, bi, locale);
            }
            // para / quote / listItem / comment
            _ => {
                let text = humanize_prose(&block.source, &dict);
                push_sentences(&mut out, &mut idx, &text, block, bi, locale);
            }
        }
    }

    out
}

fn push_sentences(
    out: &mut Vec<Sentence>,
    idx: &mut usize,
    text: &str,
    block: &Block,
    block_index: usize,
    locale: &str,
) {
    for s in split_sentences(text, locale) {
        if s.trim().is_empty() {
            continue;
        }
        out.push(Sentence {
            idx: *idx,
            text: s,
            kind: block.kind,
            start_line: block.start_line,
            end_line: block.end_line,
            block_index,
        });
        *idx += 1;
    }
}

/// Offline fallback for tables: read each row's cells, dropping the `|---|` rule.
fn table_to_speech(source: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for raw in source.split('\n') {
        let row = raw.trim_end_matches('\r').trim();
        if row.is_empty() {
            continue;
        }
        // Delimiter row: contains '-' and only `[\s|:-]`.
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
        // Heading announced.
        assert!(s[0].text.starts_with("Heading."), "{:?}", s[0].text);
        // Some sentence mentions the install step.
        assert!(s.iter().any(|x| x.text.to_lowercase().contains("install")));
    }

    #[test]
    fn table_cells_read() {
        let t = table_to_speech("| A | B |\n| --- | --- |\n| 1 | 2 |");
        assert_eq!(t, "A, B. 1, 2");
    }
}
