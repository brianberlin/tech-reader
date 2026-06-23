//! tech-reader — reads code/comments/specs aloud-but-explained.
//!
//! M2: segment a file into typed blocks, then stream an explanation of each
//! block from local Ollama (speech starts before a block finishes), falling back
//! to the deterministic humanizer when Ollama is unreachable, and speak the
//! resulting sentences gaplessly through the audio spine.

mod audio;
mod blocks;
mod humanize;
mod narrate;
mod ollama;
mod sentence;
mod tts;
mod types;
mod wav;

use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use audio::{Spine, SpineConfig};
use narrate::{stream_narration, NarrationSettings};
use ollama::OllamaConfig;
use tts::{SynthPcm, Synthesizer};

/// Narrated when no file is given.
const WELCOME_MD: &str = "\
# tech-reader

Give me a path to a source file or a markdown document, and I will read it aloud, \
explained. By default I ask a local Ollama model to rewrite each block into spoken \
prose; if Ollama is not running, I fall back to a deterministic offline humanizer, \
so I always work. Either way the audio is gapless.
";

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
const DEFAULT_MODEL: &str = "llama3.2";

struct Args {
    file: Option<PathBuf>,
    /// Print the narration text and exit (no synthesis/audio).
    text_only: bool,
    model: String,
    ollama_url: String,
}

fn parse_args() -> Args {
    let mut file = None;
    let mut text_only = false;
    let mut model = std::env::var("TECH_READER_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let mut ollama_url =
        std::env::var("TECH_READER_OLLAMA").unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string());

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--text" | "--no-audio" => text_only = true,
            "--model" | "-m" => {
                if let Some(v) = it.next() {
                    model = v;
                }
            }
            "--ollama" => {
                if let Some(v) = it.next() {
                    ollama_url = v;
                }
            }
            s if s.starts_with('-') => eprintln!("[args] ignoring unknown flag: {s}"),
            s => file = Some(PathBuf::from(s)),
        }
    }
    Args {
        file,
        text_only,
        model,
        ollama_url,
    }
}

/// Map a file extension to a language id the segmenter understands. Unknown
/// extensions return "" (prose / Markdown-ish treatment).
fn lang_from_path(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let lang = match ext.as_str() {
        "md" | "markdown" => "markdown",
        "mdx" => "mdx",
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "java" => "java",
        "cs" => "csharp",
        "rb" => "ruby",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "scala" => "scala",
        "php" => "php",
        "dart" => "dart",
        "sh" | "bash" | "zsh" => "shell",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "sql" => "sql",
        "lua" => "lua",
        "hs" => "haskell",
        "txt" | "text" => "plaintext",
        "rst" => "restructuredtext",
        _ => "",
    };
    lang.to_string()
}

/// M2 voice: the locally pre-extracted dev voice. Real first-run provisioning
/// lands in M6.
fn voice_dir() -> PathBuf {
    std::env::var("TECH_READER_VOICE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("voices/vits-piper-en_US-amy-low"))
}

fn main() -> Result<()> {
    let args = parse_args();

    let (source, lang, label) = match &args.file {
        Some(f) => (
            std::fs::read_to_string(f).with_context(|| format!("could not read {}", f.display()))?,
            lang_from_path(f),
            f.display().to_string(),
        ),
        None => (WELCOME_MD.to_string(), "markdown".to_string(), "<welcome>".to_string()),
    };

    let document = blocks::segment_blocks(&source, &lang, 1);
    if document.is_empty() {
        eprintln!("Nothing readable to narrate.");
        return Ok(());
    }
    eprintln!(
        "[narrate] {} ({}) -> {} blocks",
        label,
        if lang.is_empty() { "prose" } else { &lang },
        document.len()
    );

    let cfg = OllamaConfig::new(args.ollama_url.clone(), args.model.clone());
    let settings = NarrationSettings::default();

    // Sentence look-ahead (bounded 16): the narrator blocks when the consumer
    // falls behind — back-pressure all the way to Ollama.
    let (sent_tx, sent_rx) = sync_channel::<String>(16);

    // Narrator runs on its own OS thread with a current-thread tokio runtime
    // (the only async work is the Ollama HTTP stream).
    let narrator = thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("[narrate] could not start runtime: {e}");
                return;
            }
        };
        rt.block_on(stream_narration(&document, &settings, &cfg, "en", sent_tx));
    });

    if args.text_only {
        let mut i = 0;
        while let Ok(t) = sent_rx.recv() {
            println!("{i:>3}  {t}");
            i += 1;
        }
        let _ = narrator.join();
        return Ok(());
    }

    let vdir = voice_dir();
    let model = vdir.join("en_US-amy-low.onnx");
    let tokens = vdir.join("tokens.txt");
    let data_dir = vdir.join("espeak-ng-data");
    anyhow::ensure!(
        model.exists(),
        "voice model not found at {} — download the amy-low voice first (or set TECH_READER_VOICE_DIR)",
        model.display()
    );

    let (pcm_tx, pcm_rx) = sync_channel::<SynthPcm>(2);
    let spine = Spine::start(pcm_rx, SpineConfig::default(), PathBuf::from("out/narration.wav"))
        .context("failed to start audio spine")?;

    // Synth worker: pulls sentence texts, synthesizes ahead into the bounded
    // PCM channel (the primary back-pressure valve).
    let synth = thread::spawn(move || -> Result<()> {
        let engine = Synthesizer::new_vits(&model, &tokens, &data_dir, 1.0)
            .context("failed to create synthesizer")?;
        eprintln!("[synth] voice sample rate: {} Hz", engine.sample_rate());
        let mut i = 0;
        while let Ok(text) = sent_rx.recv() {
            let pcm = engine.synthesize(i, &text, 0, 1.0)?;
            if pcm_tx.send(pcm).is_err() {
                break; // feeder gone
            }
            i += 1;
        }
        Ok(())
    });

    let started = Instant::now();
    spine.wait_until_drained(Duration::from_secs(3600));
    let _ = narrator.join();

    let synth_result = synth.join();

    let report = spine.finish()?;
    match synth_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e.context("synth worker failed")),
        Err(_) => anyhow::bail!("synth worker panicked"),
    }

    eprintln!(
        "[done] device {} Hz x{} ch | {} frames | underruns {} | {:.1}s wall | wav {}",
        report.device_rate,
        report.channels,
        report.frames_consumed,
        report.underruns,
        started.elapsed().as_secs_f64(),
        report.wav_path,
    );
    if report.consumer_alive {
        println!(
            "Narration complete — gapless, {} mid-stream underrun frame(s).",
            report.underruns
        );
    } else {
        println!(
            "Narration rendered to {} (no live audio device in this context).",
            report.wav_path
        );
    }
    Ok(())
}
