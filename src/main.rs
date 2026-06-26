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
#[macro_use]
mod diag;
mod highlight;
mod humanize;
mod narrate;
mod ollama;
mod sentence;
mod transport;
mod tts;
mod tui;
mod types;
mod voices;
mod wav;

use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use audio::{Report, Spine, SpineConfig};
use cache::{normalize, PcmCache};
use narrate::{stream_narration, Emitter, NarrationSettings};
use ollama::OllamaConfig;
use transport::Transport;
use tts::{SynthPcm, Synthesizer};
use types::Sentence;

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
            "--help" | "-h" => {
                print_usage();
                std::process::exit(exit::OK);
            }
            "--version" | "-V" => {
                println!("tech-reader {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(exit::OK);
            }
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
            s if s.starts_with('-') => crate::diag!("[args] ignoring unknown flag: {s}"),
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

/// PCM cache byte budget from `TECH_READER_CACHE_MB` (default `DEFAULT_CACHE_MB`).
fn cache_cap_bytes() -> usize {
    std::env::var("TECH_READER_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_CACHE_MB)
        .saturating_mul(1024 * 1024)
}

fn print_usage() {
    println!(
        "tech-reader {} — reads code, comments, and specs aloud-but-explained.

USAGE:
    tech-reader [OPTIONS] [FILE]

    With no FILE, narrates a short welcome. FILE may be source code or markdown.

OPTIONS:
    -m, --model <NAME>    Ollama model for AI narration (default: {DEFAULT_MODEL})
        --ollama <URL>    Ollama base URL (default: {DEFAULT_OLLAMA_URL})
        --text            Print the narration to stdout; no synthesis or audio
    -h, --help            Print this help and exit
    -V, --version         Print the version and exit

TUI CONTROLS:
    space  pause/resume      ←/→  seek prev/next      −/+  speed
    ↑/↓    select section    ⏎    jump to selection    f    follow audio
    Tab    prose / source    w    wrap (source view)   q    quit

NOTES:
    AI narration needs a local Ollama (https://ollama.com); without it a
    deterministic offline humanizer is used. The neural voice (~64 MB) is
    downloaded and verified on first run, then everything is fully offline.

ENVIRONMENT:
    TECH_READER_MODEL, TECH_READER_OLLAMA   defaults for --model / --ollama
    TECH_READER_VOICE_DIR                   use an already-extracted voice dir
    TECH_READER_CACHE_MB                    PCM cache budget (default {DEFAULT_CACHE_MB})",
        env!("CARGO_PKG_VERSION")
    );
}

/// Initial speed multiplier from `TECH_READER_SPEED` (default 1.0), clamped to a
/// sane range so a typo can't produce a degenerate render.
fn initial_speed() -> f32 {
    std::env::var("TECH_READER_SPEED")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .map(|s| s.clamp(0.5, 2.0))
        .unwrap_or(1.0)
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

/// Install SIGINT/SIGTERM/SIGHUP handlers that set (and return) a shared
/// "interrupted" flag. The long-lived loops (TUI event loop, headless drain)
/// poll it and break to the normal teardown, which drops the audio stream **on
/// its owning thread** — the cpal stream is `!Send`, so it can only be closed
/// there, never from a handler (and a handler may do no real work anyway). The
/// terminal-key Ctrl+C is handled separately by the TUI (raw mode turns off
/// ISIG, so Ctrl+C never raises SIGINT while the TUI is up); this covers the
/// signals that *do* reach the process — terminal close / `kill` (SIGTERM,
/// SIGHUP) and SIGINT in the non-raw-mode windows (download, headless). Without
/// it those kill the process with the CoreAudio output unit still open, which
/// can wedge system audio on macOS.
#[cfg(unix)]
fn install_interrupt_flag() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    for sig in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        // A failed registration is non-fatal — we just lose graceful teardown
        // for that one signal; log it and carry on.
        if let Err(e) = signal_hook::flag::register(sig, Arc::clone(&flag)) {
            crate::diag!("[signal] could not install handler for signal {sig}: {e}");
        }
    }
    flag
}

#[cfg(not(unix))]
fn install_interrupt_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
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

    // Use the full-screen TUI only on a real terminal and not in --text mode.
    // It silences `[stage]` diagnostics so they don't corrupt the screen; if it
    // turns out there is no audio device, we re-enable them for the headless path.
    let want_tui = std::io::stdout().is_terminal() && !args.text_only;
    if want_tui {
        diag::set_quiet(true);
    }

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

    // The original document, split into display lines for the TUI's source view.
    // Normalize line endings *exactly* as `segment_blocks` does so each block's
    // 1-based line range indexes the right rows here.
    let source_lines: Arc<[String]> = source
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(String::from)
        .collect();
    crate::diag!(
        "[narrate] {} ({}) -> {} blocks",
        label,
        if lang.is_empty() { "prose" } else { &lang },
        document.len()
    );

    let cfg = OllamaConfig::new(args.ollama_url.clone(), args.model.clone());
    let settings = NarrationSettings::default();

    // Shared narration list the synth worker reads *by index* (enabling
    // random-access seek) and the TUI renders. `cancel` lets a TUI quit stop the
    // narrator promptly and the synth worker leave FFI before teardown.
    // `narrator_done` / `synth_idle` are completion signals.
    let sentences: Arc<Mutex<Vec<Sentence>>> = Arc::new(Mutex::new(Vec::new()));
    let cancel = Arc::new(AtomicBool::new(false));
    let narrator_done = Arc::new(AtomicBool::new(false));
    let synth_idle = Arc::new(AtomicBool::new(false));

    // Set by a signal handler on SIGINT/SIGTERM/SIGHUP; polled by the TUI and
    // headless loops so they break to the normal teardown (which drops the
    // audio stream) instead of the process being killed with it still open.
    let interrupted = install_interrupt_flag();

    // Transport (seek/speed) shared with the spine, synth worker, and TUI.
    // TECH_READER_SPEED sets the initial speed (mainly so headless renders can be
    // compared across speeds; the TUI ladder is the normal control).
    let transport = Arc::new(Transport::new(initial_speed()));

    // Narrator runs on its own OS thread with a current-thread tokio runtime (the
    // only async work is the Ollama HTTP stream). It appends finished sentences
    // to the shared list — no back-pressure channel, since text is cheap and a
    // fully-narrated document is what makes forward scroll + seek possible.
    let emitter = Emitter::new(Arc::clone(&sentences), Arc::clone(&cancel));
    let narrator_flag = Arc::clone(&narrator_done);
    let narrator = thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                crate::diag!("[narrate] could not start runtime: {e}");
                narrator_flag.store(true, Ordering::Release);
                return;
            }
        };
        rt.block_on(stream_narration(&document, &settings, &cfg, "en", &emitter));
        narrator_flag.store(true, Ordering::Release);
    });

    if args.text_only {
        print_narrated(&sentences, &narrator_done);
        let _ = narrator.join();
        return Ok(());
    }

    // Resolve the voice, downloading + verifying it on first run (§9.2 / §5.4).
    let voice = voices::ensure_default().map_err(|e| AppError::new(exit::VOICE, e))?;
    let model = voice.model;
    let tokens = voice.tokens;
    let data_dir = voice.data_dir;

    let cap_bytes = cache_cap_bytes();
    let fail_set = parse_fail_set();

    // Look-ahead: 2 sentences of synthesized PCM. The bounded send is the
    // primary back-pressure valve (the synth worker parks when it's full).
    let (pcm_tx, pcm_rx) = sync_channel::<SynthPcm>(2);
    let mut spine = Spine::start(
        pcm_rx,
        SpineConfig::default(),
        PathBuf::from("out/narration.wav"),
        Arc::clone(&transport),
    )
    .map_err(|e| AppError::new(exit::DEVICE, e))?;

    // The TUI runs only with a real terminal AND a live output stream (else the
    // highlight could never advance). That also decides whether the synth worker
    // idles when done (TUI, awaiting a seek) or exits (headless, so the feeder
    // ends and the drain wait completes).
    let interactive = want_tui && spine.has_output_stream();

    // Synth worker: cursor-based. Reads sentence text by index, reacting to
    // seek/speed changes (reseat the cursor; clear the cache on a speed change),
    // serves from the PCM cache or synthesizes into the bounded PCM channel, and
    // skips (with aligned silence) on failure, aborting after too many in a row.
    let synth = thread::spawn({
        let sentences = Arc::clone(&sentences);
        let cancel = Arc::clone(&cancel);
        let transport = Arc::clone(&transport);
        let narrator_done = Arc::clone(&narrator_done);
        let synth_idle = Arc::clone(&synth_idle);
        move || -> Result<()> {
            let engine = Synthesizer::new_vits(&model, &tokens, &data_dir, 1.0)
                .context("failed to create synthesizer")?;
            let rate = engine.sample_rate();
            crate::diag!("[synth] voice sample rate: {rate} Hz");

            let mut cache = PcmCache::new(cap_bytes);
            let mut cursor = 0usize;
            let mut cur_gen = transport.generation();
            let mut active_speed = transport.speed();
            let mut consecutive = 0u32;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                // React to a seek/speed change.
                let g = transport.generation();
                if g != cur_gen {
                    cur_gen = g;
                    cursor = transport.seek_target();
                    let s = transport.speed();
                    if s != active_speed {
                        active_speed = s;
                        cache.clear(); // the cache only holds the active speed (§6.4)
                    }
                }

                let len = sentences.lock().unwrap().len();
                if cursor >= len {
                    if narrator_done.load(Ordering::Acquire) {
                        synth_idle.store(true, Ordering::Relaxed);
                        if !interactive {
                            break; // headless: nothing more to produce
                        }
                    }
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                synth_idle.store(false, Ordering::Relaxed);

                let text = sentences.lock().unwrap()[cursor].text.clone();
                let pcm = match synth_one(
                    &engine, &mut cache, &fail_set, cursor, &text, rate, active_speed, g,
                    &mut consecutive,
                )? {
                    Some(p) => p,
                    None => SynthPcm::silence(cursor, rate, g),
                };
                match send_pcm(&pcm_tx, pcm, &cancel, &transport, g) {
                    SendResult::Sent => cursor += 1,
                    SendResult::Superseded => {} // a seek arrived; loop to reseat
                    SendResult::Stop => break,
                }
            }
            crate::diag!(
                "[synth] cache: {} entries, {} KB",
                cache.len(),
                cache.len_bytes() / 1024
            );
            Ok(())
        }
    });

    // TUI path: drive until quit, then tear down cleanly (§5.3) — cancel, stop
    // the stream, join the synth (so it is out of FFI), join the feeder. The
    // narrator (pure-Rust HTTP) is left for the OS to reap on exit.
    if interactive {
        let res = tui::run(
            Arc::clone(&sentences),
            Arc::clone(&source_lines),
            lang.clone(),
            &spine,
            Arc::clone(&transport),
            Arc::clone(&synth_idle),
            Arc::clone(&interrupted),
        );
        cancel.store(true, Ordering::Relaxed);
        spine.stop_audio();
        let _ = synth.join();
        let _ = spine.finish();
        return res.map_err(|e| AppError::new(exit::DEVICE, anyhow!("tui error: {e}")));
    }

    // Headless path: render to WAV / play to completion, then report.
    if want_tui {
        diag::set_quiet(false); // TTY but no device — let diagnostics through
    }
    let started = Instant::now();
    spine.wait_until_drained(Duration::from_secs(3600), &interrupted);
    cancel.store(true, Ordering::Relaxed); // release the synth worker if it idles
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

/// `--text` mode: print sentences to stdout as the narrator produces them.
fn print_narrated(sentences: &Mutex<Vec<Sentence>>, narrator_done: &AtomicBool) {
    let mut printed = 0usize;
    loop {
        let len = sentences.lock().unwrap().len();
        while printed < len {
            let t = sentences.lock().unwrap()[printed].text.clone();
            println!("{printed:>3}  {t}");
            printed += 1;
        }
        if narrator_done.load(Ordering::Acquire) && printed >= sentences.lock().unwrap().len() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Outcome of trying to hand a freshly-synthesized PCM to the feeder.
enum SendResult {
    Sent,
    /// A newer seek/speed change arrived while blocked — drop this stale PCM.
    Superseded,
    /// Cancelled, or the feeder is gone.
    Stop,
}

/// Send `pcm` to the feeder, blocking when the look-ahead is full but staying
/// responsive to cancel and to a newer seek (so a held-up stale PCM is dropped
/// rather than delaying the seek).
fn send_pcm(
    tx: &SyncSender<SynthPcm>,
    mut pcm: SynthPcm,
    cancel: &AtomicBool,
    transport: &Transport,
    generation: u64,
) -> SendResult {
    loop {
        match tx.try_send(pcm) {
            Ok(()) => return SendResult::Sent,
            Err(TrySendError::Full(p)) => {
                if cancel.load(Ordering::Relaxed) {
                    return SendResult::Stop;
                }
                if transport.generation() != generation {
                    return SendResult::Superseded;
                }
                pcm = p;
                thread::sleep(Duration::from_millis(2));
            }
            Err(TrySendError::Disconnected(_)) => return SendResult::Stop,
        }
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
    generation: u64,
    consecutive: &mut u32,
) -> Result<Option<SynthPcm>> {
    if fail_set.contains(&index) {
        crate::diag!("[synth] sentence {index} forced failure (debug) — skipping (silence).");
        return skip(consecutive, index);
    }

    let key = normalize(text);
    if let Some(samples) = cache.get(&key) {
        *consecutive = 0;
        return Ok(Some(SynthPcm::new(index, samples, rate, generation)));
    }

    match engine.synthesize(index, text, 0, speed, generation) {
        Ok(pcm) => {
            *consecutive = 0;
            cache.insert(key, Arc::clone(&pcm.samples));
            Ok(Some(pcm))
        }
        Err(e) => {
            crate::diag!("[synth] sentence {index} failed: {e:#} — skipping (silence).");
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
