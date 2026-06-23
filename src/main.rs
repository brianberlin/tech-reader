//! tech-reader — reads code/comments/specs aloud-but-explained.
//!
//! M1: segment a file into typed blocks, narrate them offline with the
//! deterministic humanizer, and speak the resulting sentences gaplessly through
//! the M0 audio spine. (AI narration via Ollama arrives in M2.)

mod audio;
mod blocks;
mod humanize;
mod narrate;
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
use narrate::{narrate_offline, NarrationSettings};
use tts::{SynthPcm, Synthesizer};

/// Hardcoded narration shown when no file is given (the M0 spine demo).
const DEMO_SENTENCES: &[&str] = &[
    "Welcome to tech reader.",
    "Give me a path to a source file or a markdown document, and I will read it aloud, explained.",
    "Right now I am running the offline humanizer: no Ollama, no cloud, everything on device.",
    "Each sentence is synthesized ahead of playback and streamed through one continuously open audio device, so there is no gap between sentences.",
];

struct Args {
    file: Option<PathBuf>,
    /// Print the narration text and exit (no synthesis/audio).
    text_only: bool,
}

fn parse_args() -> Args {
    let mut file = None;
    let mut text_only = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--text" | "--no-audio" => text_only = true,
            s if s.starts_with('-') => eprintln!("[args] ignoring unknown flag: {s}"),
            s => file = Some(PathBuf::from(s)),
        }
    }
    Args { file, text_only }
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

/// Produce the ordered narration sentence texts for the requested input.
fn build_narration(args: &Args) -> Result<Vec<String>> {
    let Some(file) = &args.file else {
        return Ok(DEMO_SENTENCES.iter().map(|s| s.to_string()).collect());
    };
    let source = std::fs::read_to_string(file)
        .with_context(|| format!("could not read {}", file.display()))?;
    let lang = lang_from_path(file);
    let blocks = blocks::segment_blocks(&source, &lang, 1);
    let sentences = narrate_offline(&blocks, "en", &NarrationSettings::default());
    eprintln!(
        "[narrate] {} ({}) -> {} blocks -> {} sentences",
        file.display(),
        if lang.is_empty() { "prose" } else { &lang },
        blocks.len(),
        sentences.len()
    );
    Ok(sentences.into_iter().map(|s| s.text).collect())
}

/// M1 voice: the locally pre-extracted dev voice. Real first-run provisioning
/// (download + sha256 verify + atomic rename) lands in M6.
fn voice_dir() -> PathBuf {
    std::env::var("TECH_READER_VOICE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("voices/vits-piper-en_US-amy-low"))
}

fn main() -> Result<()> {
    let args = parse_args();

    let texts = build_narration(&args)?;
    if texts.is_empty() {
        eprintln!("Nothing readable to narrate.");
        return Ok(());
    }

    if args.text_only {
        for (i, t) in texts.iter().enumerate() {
            println!("{i:>3}  {t}");
        }
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

    // Bounded look-ahead (depth 2): the synth worker blocks when two sentences
    // of PCM are queued — the primary back-pressure valve.
    let (pcm_tx, pcm_rx) = sync_channel::<SynthPcm>(2);

    let spine = Spine::start(pcm_rx, SpineConfig::default(), PathBuf::from("out/narration.wav"))
        .context("failed to start audio spine")?;

    let synth = thread::spawn(move || -> Result<()> {
        let engine = Synthesizer::new_vits(&model, &tokens, &data_dir, 1.0)
            .context("failed to create synthesizer")?;
        eprintln!("[synth] voice sample rate: {} Hz", engine.sample_rate());
        for (i, text) in texts.iter().enumerate() {
            let pcm = engine.synthesize(i, text, 0, 1.0)?;
            if pcm_tx.send(pcm).is_err() {
                break; // feeder gone
            }
        }
        Ok(())
    });

    // Long documents can play for many minutes; the headless rate-detection
    // bails fast when there is no real device, so this cap is just a backstop.
    let started = Instant::now();
    spine.wait_until_drained(Duration::from_secs(3600));

    match synth.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e.context("synth worker failed")),
        Err(_) => anyhow::bail!("synth worker panicked"),
    }

    let report = spine.finish()?;
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
