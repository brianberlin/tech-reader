//! The gapless audio spine.
//!
//! One persistently-open `cpal` output stream is fed from a wait-free `rtrb`
//! ring buffer. A **feeder** thread drains synthesized PCM off a bounded
//! channel, resamples it to the device rate, applies a short boundary ramp,
//! inserts the inter-sentence silence, and pushes it into the ring. The `cpal`
//! real-time callback does **nothing but drain the ring** into the output
//! buffer (no alloc, no lock, no syscall) — on underrun it writes silence and
//! bumps a counter.
//!
//! This removes every per-sentence cost of the old cold-spawn loop: the device
//! opens once, and CPU-heavy synthesis runs *ahead* of playback into the buffer.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::tts::SynthPcm;
use crate::wav;

/// Tunables for the spine. Durations are in milliseconds and converted to sample
/// counts at the *device* rate.
#[derive(Clone, Copy)]
pub struct SpineConfig {
    /// Intentional silence between sentences (prosodic spacing, not gap-hiding).
    pub silence_ms: u32,
    /// Target audio held in the ring buffer.
    pub ring_ms: u32,
    /// Linear fade in/out at each sentence edge to guarantee click-free joins.
    pub ramp_ms: u32,
    /// How much audio to buffer before unmuting the callback (avoids a startup
    /// underrun before the ring has filled).
    pub prebuffer_ms: u32,
}

impl Default for SpineConfig {
    fn default() -> Self {
        Self {
            silence_ms: 120,
            ring_ms: 300,
            ramp_ms: 3,
            prebuffer_ms: 150,
        }
    }
}

/// State shared between the main thread, the feeder, and the RT callback.
struct Shared {
    /// Interleaved samples enqueued by the feeder.
    samples_pushed: AtomicU64,
    /// Interleaved samples actually drained by the callback.
    samples_consumed: AtomicU64,
    /// Interleaved silence samples written on underrun.
    underruns: AtomicU64,
    /// Set once enough audio is buffered; gates the callback (silent until set).
    started: AtomicBool,
    /// Set when no device is draining the ring (headless): feeder stops pushing.
    consumer_dead: AtomicBool,
    /// Set when the feeder has consumed its entire input.
    feeder_done: AtomicBool,
}

pub struct Spine {
    stream: Option<cpal::Stream>,
    feeder: Option<JoinHandle<Vec<f32>>>,
    shared: Arc<Shared>,
    device_rate: u32,
    channels: u16,
    ring_capacity: u64,
    wav_path: PathBuf,
}

/// End-of-run diagnostics (frames are per-channel; interleaved / channels).
pub struct Report {
    pub device_rate: u32,
    pub channels: u16,
    pub frames_pushed: u64,
    pub frames_consumed: u64,
    pub underruns: u64,
    pub consumer_alive: bool,
    pub wav_path: String,
}

impl Spine {
    /// Open the default output device and start the stream, spawning the feeder.
    /// If no usable audio output is available (any step fails — no device, bad
    /// format, build/play error), degrade to **WAV-only** rendering so narration
    /// always completes; on a real session with a working device it plays live.
    pub fn start(pcm_rx: Receiver<SynthPcm>, cfg: SpineConfig, wav_path: PathBuf) -> Result<Spine> {
        // Query the device config first so the ring can be sized to its rate.
        let device_cfg = try_open_device();
        let (device_rate, channels) = match &device_cfg {
            Ok((_, _, rate, ch)) => (*rate, *ch),
            Err(_) => (44_100, 2), // sane defaults for WAV-only rendering
        };

        let ring_capacity = ((device_rate as u64) * (channels as u64) * (cfg.ring_ms as u64) / 1000)
            .max(channels as u64 * 64) as usize;
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_capacity);

        let shared = Arc::new(Shared {
            samples_pushed: AtomicU64::new(0),
            samples_consumed: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            started: AtomicBool::new(false),
            consumer_dead: AtomicBool::new(false),
            feeder_done: AtomicBool::new(false),
        });

        let stream = match device_cfg {
            Ok((device, config, _, _)) => match build_and_play(&device, config, &shared, consumer) {
                Ok(s) => Some(s),
                Err(e) => {
                    eprintln!("[spine] could not start audio output ({e}); rendering to WAV only.");
                    mark_no_device(&shared);
                    None
                }
            },
            Err(e) => {
                eprintln!("[spine] no usable audio output ({e}); rendering to WAV only.");
                drop(consumer);
                mark_no_device(&shared);
                None
            }
        };

        let feeder_shared = Arc::clone(&shared);
        let feeder =
            thread::spawn(move || feed(pcm_rx, producer, device_rate, channels, cfg, feeder_shared));

        Ok(Spine {
            stream,
            feeder: Some(feeder),
            shared,
            device_rate,
            channels,
            ring_capacity: ring_capacity as u64,
            wav_path,
        })
    }

    /// Block until the ring is fully drained, or until it's clear no device is
    /// draining it (headless) — in which case the feeder is told to stop pushing
    /// so it can finish teeing the WAV.
    pub fn wait_until_drained(&self, max: Duration) {
        let start = Instant::now();
        // 60 ms of audio: "the device clearly pulled real samples".
        let min_live = (self.device_rate as u64 * self.channels as u64 * 60 / 1000).max(1);

        let mut started_at: Option<Instant> = None;
        let mut live = false; // latched once the device proves it drains
        let mut last_consumed = 0u64;
        let mut last_change = Instant::now();

        loop {
            let consumed = self.shared.samples_consumed.load(Relaxed);
            let pushed = self.shared.samples_pushed.load(Relaxed);

            if self.shared.feeder_done.load(Relaxed) && consumed >= pushed {
                break; // fully drained
            }
            if self.shared.consumer_dead.load(Relaxed) {
                break;
            }
            if start.elapsed() > max {
                eprintln!("[spine] drain wait hit the {}s cap.", max.as_secs());
                self.shared.consumer_dead.store(true, Relaxed);
                break;
            }

            if self.shared.started.load(Relaxed) && started_at.is_none() {
                started_at = Some(Instant::now());
                last_change = Instant::now();
            }
            if consumed != last_consumed {
                last_consumed = consumed;
                last_change = Instant::now();
                if consumed >= min_live {
                    live = true;
                }
            }

            if let Some(sa) = started_at {
                if !live {
                    // Probe: a real device drains the prebuffer within a few
                    // seconds; a headless context never pulls.
                    if sa.elapsed() > Duration::from_secs(3) {
                        eprintln!("[spine] no audio drained after start — no usable output device.");
                        self.shared.consumer_dead.store(true, Relaxed);
                        break;
                    }
                } else if last_change.elapsed() > Duration::from_secs(4) {
                    // Live but consumption stalled. Distinguish a dead device
                    // (a full ring nobody is draining) from a legitimate gap
                    // (ring empty, waiting on synthesis / Ollama).
                    let occupancy = pushed.saturating_sub(consumed);
                    if occupancy * 2 >= self.ring_capacity {
                        eprintln!(
                            "[spine] device stopped draining a buffered ring — assuming disconnect."
                        );
                        self.shared.consumer_dead.store(true, Relaxed);
                        break;
                    }
                    // Ring is near-empty: waiting on more audio. Keep waiting,
                    // but reset the window so the next stall is judged fresh.
                    last_change = Instant::now();
                }
            }

            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Stop the device, join the feeder, write the teed WAV, and return stats.
    pub fn finish(mut self) -> Result<Report> {
        if let Some(stream) = self.stream.take() {
            drop(stream); // quiesce the callback first
        }
        let wav_buf = match self.feeder.take() {
            Some(h) => h.join().map_err(|_| anyhow!("feeder thread panicked"))?,
            None => Vec::new(),
        };
        wav::write_i16_wav(&self.wav_path, &wav_buf, self.device_rate, self.channels)
            .with_context(|| format!("failed to write {}", self.wav_path.display()))?;

        let ch = self.channels.max(1) as u64;
        Ok(Report {
            device_rate: self.device_rate,
            channels: self.channels,
            frames_pushed: self.shared.samples_pushed.load(Relaxed) / ch,
            frames_consumed: self.shared.samples_consumed.load(Relaxed) / ch,
            underruns: self.shared.underruns.load(Relaxed) / ch,
            consumer_alive: !self.shared.consumer_dead.load(Relaxed),
            wav_path: self.wav_path.display().to_string(),
        })
    }
}

/// Query the default output device and its f32 config (rate, channels).
fn try_open_device() -> Result<(cpal::Device, cpal::StreamConfig, u32, u16)> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default output device")?;
    let supported = device
        .default_output_config()
        .context("no default output config")?;
    anyhow::ensure!(
        supported.sample_format() == cpal::SampleFormat::F32,
        "default output format is {:?}, not f32",
        supported.sample_format()
    );
    let config: cpal::StreamConfig = supported.config();
    let rate = config.sample_rate; // cpal 0.18: SampleRate = u32
    let channels = config.channels;
    Ok((device, config, rate, channels))
}

/// Build and start the output stream whose RT callback only drains the ring
/// (no alloc/lock/IO) into the output buffer, writing silence on underrun.
fn build_and_play(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: &Arc<Shared>,
    mut consumer: rtrb::Consumer<f32>,
) -> Result<cpal::Stream> {
    let cb = Arc::clone(shared);
    let data_cb = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        if !cb.started.load(Relaxed) {
            data.iter_mut().for_each(|s| *s = 0.0);
            return;
        }
        let mut consumed = 0u64;
        let mut under = 0u64;
        for s in data.iter_mut() {
            match consumer.pop() {
                Ok(v) => {
                    *s = v;
                    consumed += 1;
                }
                Err(_) => {
                    *s = 0.0;
                    under += 1;
                }
            }
        }
        if consumed > 0 {
            cb.samples_consumed.fetch_add(consumed, Relaxed);
        }
        if under > 0 {
            cb.underruns.fetch_add(under, Relaxed);
        }
    };
    let err_cb = |err| eprintln!("[cpal] stream error: {err}");
    let stream = device
        .build_output_stream(config, data_cb, err_cb, None) // cpal 0.18: by value
        .context("failed to build output stream")?;
    stream.play().context("failed to start output stream")?;
    Ok(stream)
}

/// No usable device: pre-flag dead + started so the feeder tees the WAV without
/// blocking on a ring nobody drains.
fn mark_no_device(shared: &Arc<Shared>) {
    shared.consumer_dead.store(true, Relaxed);
    shared.started.store(true, Relaxed);
}

/// Feeder thread body: PCM channel -> resample -> ring (+ WAV tee).
fn feed(
    pcm_rx: Receiver<SynthPcm>,
    mut producer: rtrb::Producer<f32>,
    device_rate: u32,
    channels: u16,
    cfg: SpineConfig,
    shared: Arc<Shared>,
) -> Vec<f32> {
    let silence_frames = (device_rate as u64 * cfg.silence_ms as u64 / 1000) as usize;
    let ramp_frames = (device_rate as u64 * cfg.ramp_ms as u64 / 1000).max(1) as usize;
    let prebuffer_samples = device_rate as u64 * channels as u64 * cfg.prebuffer_ms as u64 / 1000;
    let mut started = false;

    // Pre-reserve the WAV tee so a mid-stream Vec realloc never stalls the feeder
    // (which would starve the ring and cause an underrun). Dev-only; production
    // has no tee. Generous headroom; grows if exceeded.
    let mut wav_buf: Vec<f32> = Vec::with_capacity(device_rate as usize * channels as usize * 90);

    let trim_margin = (device_rate as usize * 10 / 1000).max(1); // ~10 ms keepout

    while let Ok(pcm) = pcm_rx.recv() {
        let resampled = resample_linear(&pcm.samples, pcm.sample_rate, device_rate);
        // Trim the voice's own leading/trailing near-silence so the only gap
        // between sentences is our exact, tunable inter-sentence silence (N1).
        let (s, e) = speech_bounds(&resampled, 0.008, trim_margin);
        let mut mono = if e > s {
            resampled[s..e].to_vec()
        } else {
            Vec::new()
        };
        apply_ramp(&mut mono, ramp_frames);

        // Build the interleaved frame buffer for this sentence + its trailing
        // inter-sentence silence, once, then bulk-push it.
        let mut inter = Vec::with_capacity((mono.len() + silence_frames) * channels as usize);
        for &m in &mono {
            for _ in 0..channels {
                inter.push(m);
            }
        }
        inter.resize(inter.len() + silence_frames * channels as usize, 0.0);

        wav_buf.extend_from_slice(&inter);
        push_slice(&mut producer, &inter, &shared, prebuffer_samples, &mut started);
    }

    // Short inputs may never reach the prebuffer target; ensure playback starts.
    shared.started.store(true, Relaxed);
    shared.feeder_done.store(true, Relaxed);
    wav_buf
}

/// Bulk-push interleaved samples into the ring, blocking (back-pressure) when
/// full. Accounts `samples_pushed` per committed chunk and flips `started` once
/// the prebuffer is buffered — so the callback can begin draining before the
/// whole (possibly larger-than-ring) sentence is pushed. Bails if the consumer
/// is declared dead so the feeder can keep teeing the WAV.
fn push_slice(
    producer: &mut rtrb::Producer<f32>,
    data: &[f32],
    shared: &Shared,
    prebuffer_samples: u64,
    started: &mut bool,
) {
    let mut i = 0;
    while i < data.len() {
        if shared.consumer_dead.load(Relaxed) {
            return;
        }
        let avail = producer.slots();
        if avail == 0 {
            thread::sleep(Duration::from_micros(200));
            continue;
        }
        let n = avail.min(data.len() - i);
        match producer.write_chunk(n) {
            Ok(mut chunk) => {
                let (first, second) = chunk.as_mut_slices();
                let mut k = 0;
                for slot in first.iter_mut() {
                    *slot = data[i + k];
                    k += 1;
                }
                for slot in second.iter_mut() {
                    *slot = data[i + k];
                    k += 1;
                }
                chunk.commit_all();
                i += n;
                let pushed = shared.samples_pushed.fetch_add(n as u64, Relaxed) + n as u64;
                if !*started && pushed >= prebuffer_samples {
                    shared.started.store(true, Relaxed);
                    *started = true;
                }
            }
            Err(_) => thread::sleep(Duration::from_micros(200)),
        }
    }
}

/// Linear-interpolation resampler (mono). M0-grade; replaced by `rubato` in M6.
fn resample_linear(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if input.is_empty() || src_rate == dst_rate {
        return input.to_vec();
    }
    let ratio = dst_rate as f64 / src_rate as f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 / ratio;
        let idx = pos.floor() as usize;
        let frac = (pos - idx as f64) as f32;
        let a = input.get(idx).copied().unwrap_or(0.0);
        let b = input.get(idx + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * frac);
    }
    out
}

/// Find the speech span of a mono buffer: first/last samples above `threshold`,
/// padded by `margin` samples. Returns `(start, end_exclusive)`, or `(0, 0)` if
/// the whole buffer is below threshold.
fn speech_bounds(x: &[f32], threshold: f32, margin: usize) -> (usize, usize) {
    let first = x.iter().position(|&v| v.abs() > threshold);
    let last = x.iter().rposition(|&v| v.abs() > threshold);
    match (first, last) {
        (Some(f), Some(l)) => (f.saturating_sub(margin), (l + 1 + margin).min(x.len())),
        _ => (0, 0),
    }
}

/// Short linear fade in/out at the sentence edges (click-free joins).
fn apply_ramp(mono: &mut [f32], ramp: usize) {
    let n = mono.len();
    if n == 0 {
        return;
    }
    let r = ramp.min(n / 2);
    for i in 0..r {
        let g = i as f32 / r as f32;
        mono[i] *= g;
        mono[n - 1 - i] *= g;
    }
}
