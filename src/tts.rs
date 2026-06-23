//! In-process neural TTS via the official k2-fsa `sherpa-onnx` crate.
//!
//! A [`Synthesizer`] owns a sherpa-onnx `OfflineTts` handle (a wrapper over a
//! C++ object) and turns one sentence at a time into owned mono f32 PCM. The
//! handle is **not** `Send`, so a `Synthesizer` is constructed and used on a
//! single thread (the synth worker).

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use sherpa_onnx::{
    GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsModelConfig,
    OfflineTtsVitsModelConfig,
};

/// One synthesized sentence: mono f32 PCM at `sample_rate`.
///
/// The samples are shared via `Arc` so the PCM cache (§7.1.6) can keep a copy
/// while the same buffer flows on to the feeder, with no deep copy on a cache
/// hit and no deep copy to populate the cache.
pub struct SynthPcm {
    /// Index of the sentence in the narration stream (for boundary tracking).
    pub sentence_index: usize,
    /// Mono PCM samples in `[-1.0, 1.0]`.
    pub samples: Arc<[f32]>,
    /// The voice's native output sample rate (Hz). Never assume 22050.
    pub sample_rate: u32,
}

impl SynthPcm {
    pub fn new(sentence_index: usize, samples: Arc<[f32]>, sample_rate: u32) -> Self {
        Self {
            sentence_index,
            samples,
            sample_rate,
        }
    }

    /// A skipped/failed sentence: no samples. The feeder still inserts the
    /// inter-sentence silence for it, so the sentence index stays aligned with
    /// the audio downstream (§5.4 — "emits a short silence so the index stays
    /// aligned").
    pub fn silence(sentence_index: usize, sample_rate: u32) -> Self {
        Self {
            sentence_index,
            samples: Arc::from(Vec::new()),
            sample_rate,
        }
    }
}

/// Owns a sherpa-onnx `OfflineTts`. Construct and call on one thread.
pub struct Synthesizer {
    tts: OfflineTts,
}

impl Synthesizer {
    /// Build a VITS/Piper synthesizer from a voice directory's three artifacts:
    /// the `.onnx` model, its `tokens.txt`, and the `espeak-ng-data/` phonemizer
    /// data directory. `length_scale` is the base pace (1.0 = normal; >1 slower).
    pub fn new_vits(
        model: &Path,
        tokens: &Path,
        data_dir: &Path,
        length_scale: f32,
    ) -> Result<Self> {
        let vits = OfflineTtsVitsModelConfig {
            model: Some(path_str(model)?),
            tokens: Some(path_str(tokens)?),
            data_dir: Some(path_str(data_dir)?),
            noise_scale: 0.667,
            noise_scale_w: 0.8,
            length_scale,
            ..Default::default()
        };
        let model_cfg = OfflineTtsModelConfig {
            vits,
            num_threads: 2,
            debug: false,
            provider: Some("cpu".to_string()),
            ..Default::default()
        };
        let config = OfflineTtsConfig {
            model: model_cfg,
            ..Default::default()
        };
        let tts = OfflineTts::create(&config).ok_or_else(|| {
            anyhow!(
                "OfflineTts::create returned None — check model/tokens/data_dir paths:\n  model: {}\n  tokens: {}\n  data_dir: {}",
                model.display(),
                tokens.display(),
                data_dir.display()
            )
        })?;
        Ok(Self { tts })
    }

    /// The engine's output sample rate (Hz), read from the loaded model.
    pub fn sample_rate(&self) -> u32 {
        self.tts.sample_rate() as u32
    }

    /// Synthesize one sentence to owned mono f32 PCM.
    ///
    /// `sid` selects the speaker (0 for single-speaker voices). `speed` is the
    /// pitch-preserving pace multiplier (>1 faster) — it re-times the VITS
    /// durations, so it cannot be applied to already-rendered audio.
    pub fn synthesize(&self, index: usize, text: &str, sid: i32, speed: f32) -> Result<SynthPcm> {
        let cfg = GenerationConfig {
            sid,
            speed,
            ..Default::default()
        };
        let audio = self
            .tts
            // The progress callback is unused here; `None` needs a concrete type
            // because the compiler can't infer the closure type `F`.
            .generate_with_config(text, &cfg, None::<fn(&[f32], f32) -> bool>)
            .ok_or_else(|| anyhow!("generate_with_config returned None for sentence {index}"))?;
        Ok(SynthPcm {
            sentence_index: index,
            // `samples()` borrows a C-owned buffer; `Arc::from(&[f32])` copies it
            // once into an owned, shareable allocation.
            samples: Arc::from(audio.samples()),
            sample_rate: audio.sample_rate() as u32,
        })
    }
}

fn path_str(p: &Path) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("non-UTF8 path: {}", p.display()))
}
