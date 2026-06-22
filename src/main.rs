//! tech-reader — M0 audio-spine prototype.
//!
//! De-risks the heart of the rewrite: in-process sherpa-onnx synthesis streamed
//! through a wait-free ring buffer into one persistently-open cpal device, with
//! synthesis running *ahead* of playback so there is no cold-spawn gap between
//! sentences. A handful of hardcoded sentences are synthesized and played
//! gaplessly; the exact device-bound stream is also teed to `out/m0.wav`.

mod audio;
mod tts;
mod wav;

use std::path::PathBuf;
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use audio::{Spine, SpineConfig};
use tts::{SynthPcm, Synthesizer};

/// Hardcoded narration for the M0 prototype (no segmenter/narrator yet).
const SENTENCES: &[&str] = &[
    "Welcome to tech reader.",
    "This is the M zero audio spine, running entirely on device.",
    "Each sentence is synthesized in process by sherpa onnx, then streamed through a lock free ring buffer into one continuously open audio device.",
    "Because synthesis runs ahead of playback, there is no cold spawn gap between sentences.",
    "Between sentences you should hear a short, deliberate pause, not an awkward silence.",
    "If this sounds smooth and continuous, the gapless architecture works.",
];

/// M0 voice: the locally pre-extracted dev voice. Real first-run provisioning
/// (download + sha256 verify + atomic rename) lands in M6.
fn voice_dir() -> PathBuf {
    std::env::var("TECH_READER_VOICE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("voices/vits-piper-en_US-amy-low"))
}

fn main() -> Result<()> {
    let vdir = voice_dir();
    let model = vdir.join("en_US-amy-low.onnx");
    let tokens = vdir.join("tokens.txt");
    let data_dir = vdir.join("espeak-ng-data");
    anyhow::ensure!(
        model.exists(),
        "voice model not found at {} — download the amy-low voice first",
        model.display()
    );

    // Bounded look-ahead (depth 2): the synth worker blocks when two sentences
    // of PCM are queued — the primary back-pressure valve.
    let (pcm_tx, pcm_rx) = sync_channel::<SynthPcm>(2);

    // Audio spine: opens the device once, spawns the feeder, starts the stream.
    let spine = Spine::start(pcm_rx, SpineConfig::default(), PathBuf::from("out/m0.wav"))
        .context("failed to start audio spine")?;

    // Synth worker: owns the (non-Send) OfflineTts and synthesizes ahead into
    // the bounded channel on its own OS thread (blocking CPU work).
    let synth = thread::spawn(move || -> Result<()> {
        let engine = Synthesizer::new_vits(&model, &tokens, &data_dir, 1.0)
            .context("failed to create synthesizer")?;
        eprintln!("[synth] voice sample rate: {} Hz", engine.sample_rate());
        for (i, sentence) in SENTENCES.iter().enumerate() {
            let t0 = Instant::now();
            let pcm = engine.synthesize(i, sentence, 0, 1.0)?;
            eprintln!(
                "[synth] sentence {i}: {} samples @ {} Hz in {} ms",
                pcm.samples.len(),
                pcm.sample_rate,
                t0.elapsed().as_millis()
            );
            if pcm_tx.send(pcm).is_err() {
                break; // feeder gone
            }
        }
        Ok(())
    });

    // Wait for playback to fully drain (with a safety cap for headless runs).
    spine.wait_until_drained(Duration::from_secs(60));

    match synth.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e.context("synth worker failed")),
        Err(_) => anyhow::bail!("synth worker panicked"),
    }

    let report = spine.finish()?;
    eprintln!(
        "[m0] device {} Hz x{} ch | pushed {} frames | consumed {} frames | underruns {} | wav {}",
        report.device_rate,
        report.channels,
        report.frames_pushed,
        report.frames_consumed,
        report.underruns,
        report.wav_path,
    );
    if report.consumer_alive {
        println!(
            "M0 OK — gapless stream drained with {} mid-stream underrun frame(s). Rendered {}.",
            report.underruns, report.wav_path
        );
    } else {
        println!(
            "M0 (headless) — no live device drain detected; rendered {} to inspect/listen.",
            report.wav_path
        );
    }
    Ok(())
}
