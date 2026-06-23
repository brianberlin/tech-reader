//! tech-reader — reads code/comments/specs aloud-but-explained.
//!
//! M3: the M2 streaming-narration pipeline plus look-ahead/back-pressure
//! hardening — a byte-capped PCM cache, synthesis-failure handling that skips a
//! sentence (short silence) without desyncing the index, a streaming WAV tee so
//! memory stays bounded under a long file, an out-of-band underrun monitor, and
//! the §5.4 CLI exit codes.

mod audio;
mod blocks;
mod cache;
mod humanize;
mod narrate;
mod ollama;
mod sentence;
mod tts;
mod types;
mod wav;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use audio::{Report, Spine, SpineConfig};
use cache::{normalize, PcmCache};
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

/// Default PCM cache budget (MB of voice-native f32 samples). ~13 min of unique
/// audio at 22050 Hz mono; override with `TECH_READER_CACHE_MB`.
const DEFAULT_CACHE_MB: usize = 64;

/// Abort synthesis after this many failures in a row (§5.4 → exit 4).
const MAX_CONSECUTIVE_SYNTH_FAILURES: u32 = 3;

/// Process exit codes (DESIGN-REWRITE §5.4).
mod exit {
    pub const OK: i32 = 0;
    pub const USAGE: i32 = 1;
    pub const INPUT_UNREADABLE: i32 = 2;
    pub const VOICE: i32 = 3;
    pub const SYNTH: i32 = 4;
    pub const DEVICE: i32 = 5;
}

/// An error carrying the process exit code to report it with.
struct AppError {
    code: i32,
    err: anyhow::Error,
}

impl AppError {
    fn new(code: i32, err: anyhow::Error) -> Self {
        Self { code, err }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.err)
    }
}

struct Args {
    file: Option<PathBuf>,
    /// Print the narration text and exit (no synthesis/audio).
    text_only: bool,
    model: String,
    ollama_url: String,
}

fn parse_args() -> std::result::Result<Args, AppError> {
    let mut file = None;
    let mut text_only = false;
    let mut model = std::env::var("TECH_READER_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let mut ollama_url =
        std::env::var("TECH_READER_OLLAMA").unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string());

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--text" | "--no-audio" => text_only = true,
            "--model" | "-m" => match it.next() {
                Some(v) => model = v,
                None => return Err(AppError::new(exit::USAGE, anyhow!("--model requires a value"))),
            },
            "--ollama" => match it.next() {
                Some(v) => ollama_url = v,
                None => {
                    return Err(AppError::new(exit::USAGE, anyhow!("--ollama requires a value")))
                }
            },
            s if s.starts_with('-') => eprintln!("[args] ignoring unknown flag: {s}"),
            s => file = Some(PathBuf::from(s)),
        }
    }
    Ok(Args {
        file,
        text_only,
        model,
        ollama_url,
    })
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

/// M3 voice: the locally pre-extracted dev voice. Real first-run provisioning
/// lands in M6.
fn voice_dir() -> PathBuf {
    std::env::var("TECH_READER_VOICE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("voices/vits-piper-en_US-amy-low"))
}

/// PCM cache byte budget from `TECH_READER_CACHE_MB` (default `DEFAULT_CACHE_MB`).
fn cache_cap_bytes() -> usize {
    std::env::var("TECH_READER_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_CACHE_MB)
        .saturating_mul(1024 * 1024)
}

/// Debug hook: comma-separated sentence indices to force a synthesis failure on,
/// for exercising the §5.4 skip/abort paths (e.g. `TECH_READER_FAIL_SENTENCE=2`).
fn parse_fail_set() -> HashSet<usize> {
    std::env::var("TECH_READER_FAIL_SENTENCE")
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|s| s.trim().parse::<usize>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn main() {
    let code = match run() {
        Ok(()) => exit::OK,
        Err(e) => {
            eprintln!("error: {e}");
            e.code
        }
    };
    std::process::exit(code);
}

fn run() -> std::result::Result<(), AppError> {
    let args = parse_args()?;

    let (source, lang, label) = match &args.file {
        Some(f) => (
            std::fs::read_to_string(f).map_err(|e| {
                AppError::new(
                    exit::INPUT_UNREADABLE,
                    anyhow!("could not read {}: {e}", f.display()),
                )
            })?,
            lang_from_path(f),
            f.display().to_string(),
        ),
        None => (
            WELCOME_MD.to_string(),
            "markdown".to_string(),
            "<welcome>".to_string(),
        ),
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
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
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
    if !model.exists() {
        return Err(AppError::new(
            exit::VOICE,
            anyhow!(
                "voice model not found at {} — download the amy-low voice first \
                 (or set TECH_READER_VOICE_DIR)",
                model.display()
            ),
        ));
    }

    let cap_bytes = cache_cap_bytes();
    let fail_set = parse_fail_set();
    let length_scale = 1.0f32; // base pace; the speed ladder lands in M5
    let speed = 1.0f32;

    // Look-ahead: 2 sentences of synthesized PCM. The bounded send is the
    // primary back-pressure valve (the synth worker parks when it's full).
    let (pcm_tx, pcm_rx) = sync_channel::<SynthPcm>(2);
    let spine = Spine::start(pcm_rx, SpineConfig::default(), PathBuf::from("out/narration.wav"))
        .map_err(|e| AppError::new(exit::DEVICE, e))?;

    // Synth worker: pulls sentence texts, serves them from the PCM cache or
    // synthesizes ahead into the bounded PCM channel, skipping (with aligned
    // silence) on failure and aborting after too many in a row.
    let synth = thread::spawn(move || -> Result<()> {
        let engine = Synthesizer::new_vits(&model, &tokens, &data_dir, length_scale)
            .context("failed to create synthesizer")?;
        let rate = engine.sample_rate();
        eprintln!("[synth] voice sample rate: {rate} Hz");

        let mut cache = PcmCache::new(cap_bytes);
        let mut i = 0usize;
        let mut consecutive = 0u32;
        while let Ok(text) = sent_rx.recv() {
            let pcm = match synth_one(
                &engine,
                &mut cache,
                &fail_set,
                i,
                &text,
                rate,
                speed,
                &mut consecutive,
            )? {
                Some(p) => p,
                None => SynthPcm::silence(i, rate),
            };
            if pcm_tx.send(pcm).is_err() {
                break; // feeder gone
            }
            i += 1;
        }
        eprintln!(
            "[synth] cache: {} entries, {} KB",
            cache.len(),
            cache.len_bytes() / 1024
        );
        Ok(())
    });

    let started = Instant::now();
    spine.wait_until_drained(Duration::from_secs(3600));
    let _ = narrator.join();
    let synth_result = synth.join();

    let report = spine
        .finish()
        .map_err(|e| AppError::new(exit::DEVICE, e))?;
    print_report(&report, started);

    match synth_result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(AppError::new(exit::SYNTH, e.context("synthesis failed"))),
        Err(_) => Err(AppError::new(exit::SYNTH, anyhow!("synth worker panicked"))),
    }
}

/// Resolve one sentence to PCM. Returns `Ok(Some)` on a cache hit or fresh
/// synthesis, `Ok(None)` to signal "skip with aligned silence", and `Err` only
/// to abort after `MAX_CONSECUTIVE_SYNTH_FAILURES` failures in a row.
#[allow(clippy::too_many_arguments)]
fn synth_one(
    engine: &Synthesizer,
    cache: &mut PcmCache,
    fail_set: &HashSet<usize>,
    index: usize,
    text: &str,
    rate: u32,
    speed: f32,
    consecutive: &mut u32,
) -> Result<Option<SynthPcm>> {
    if fail_set.contains(&index) {
        eprintln!("[synth] sentence {index} forced failure (debug) — skipping (silence).");
        return skip(consecutive, index);
    }

    let key = normalize(text);
    if let Some(samples) = cache.get(&key) {
        *consecutive = 0;
        return Ok(Some(SynthPcm::new(index, samples, rate)));
    }

    match engine.synthesize(index, text, 0, speed) {
        Ok(pcm) => {
            *consecutive = 0;
            cache.insert(key, Arc::clone(&pcm.samples));
            Ok(Some(pcm))
        }
        Err(e) => {
            eprintln!("[synth] sentence {index} failed: {e:#} — skipping (silence).");
            skip(consecutive, index)
        }
    }
}

/// Count a synthesis failure: abort (`Err`) past the consecutive limit, else
/// signal a silent skip (`Ok(None)`).
fn skip(consecutive: &mut u32, index: usize) -> Result<Option<SynthPcm>> {
    *consecutive += 1;
    let n = *consecutive;
    if n >= MAX_CONSECUTIVE_SYNTH_FAILURES {
        anyhow::bail!("aborting after {n} consecutive synthesis failures (last at sentence {index})");
    }
    Ok(None)
}

fn print_report(report: &Report, started: Instant) {
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
}
