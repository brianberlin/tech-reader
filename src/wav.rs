//! Minimal 16-bit PCM WAV writer — used to tee the exact interleaved stream the
//! audio spine sends to the device, so the gapless output can be inspected and
//! played back independently of whether a live audio device is available.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::Result;

/// Write interleaved f32 samples in `[-1.0, 1.0]` as a 16-bit PCM WAV.
pub fn write_i16_wav(path: &Path, samples: &[f32], sample_rate: u32, channels: u16) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut w = BufWriter::new(File::create(path)?);

    let bits_per_sample: u16 = 16;
    let block_align: u16 = channels * (bits_per_sample / 8);
    let byte_rate: u32 = sample_rate * block_align as u32;
    let data_len: u32 = (samples.len() as u32) * (bits_per_sample / 8) as u32;

    // RIFF / WAVE header
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVE")?;

    // fmt chunk (PCM)
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // fmt chunk size
    w.write_all(&1u16.to_le_bytes())?; // audio format = PCM
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&bits_per_sample.to_le_bytes())?;

    // data chunk
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        w.write_all(&v.to_le_bytes())?;
    }
    w.flush()?;
    Ok(())
}
