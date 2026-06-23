//! Streaming 16-bit PCM WAV writer — tees the exact interleaved stream the audio
//! spine sends to the device, so the gapless output can be inspected and played
//! back independently of whether a live audio device is available.
//!
//! It writes **incrementally** to disk (a placeholder header up front, samples
//! as they arrive, then the real sizes patched in on [`WavWriter::finalize`]) so
//! memory stays bounded under a long document (N7) — the whole narration is
//! never buffered in RAM. A run killed before `finalize` leaves a header with
//! zero sizes; that is acceptable for a dev artifact.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result};

/// Bytes 4..8 (RIFF chunk size) and 40..44 (data chunk size) in a canonical
/// 44-byte PCM WAV header.
const RIFF_SIZE_OFFSET: u64 = 4;
const DATA_SIZE_OFFSET: u64 = 40;
const HEADER_BYTES: u32 = 44;

pub struct WavWriter {
    file: BufWriter<File>,
    /// Interleaved 16-bit samples written so far (2 bytes each).
    samples_written: u64,
}

impl WavWriter {
    /// Create `path` and write a placeholder header. Buffered generously so
    /// per-sentence writes rarely hit a syscall and never stall the feeder.
    pub fn create(path: &Path, sample_rate: u32, channels: u16) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
        let mut file = BufWriter::with_capacity(256 * 1024, file);
        write_header(&mut file, sample_rate, channels, 0)?;
        Ok(Self {
            file,
            samples_written: 0,
        })
    }

    /// Append interleaved f32 samples in `[-1.0, 1.0]` as 16-bit LE PCM.
    pub fn write_frames(&mut self, interleaved: &[f32]) -> Result<()> {
        let mut buf = [0u8; 4096];
        let mut bi = 0usize;
        for &s in interleaved {
            let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            let b = v.to_le_bytes();
            buf[bi] = b[0];
            buf[bi + 1] = b[1];
            bi += 2;
            if bi == buf.len() {
                self.file.write_all(&buf)?;
                bi = 0;
            }
        }
        if bi > 0 {
            self.file.write_all(&buf[..bi])?;
        }
        self.samples_written += interleaved.len() as u64;
        Ok(())
    }

    /// Flush, then patch the RIFF and data chunk sizes now that they are known.
    pub fn finalize(mut self) -> Result<()> {
        self.file.flush()?;
        let data_len = (self.samples_written * 2).min(u32::MAX as u64) as u32;
        let riff_len = (HEADER_BYTES - 8).saturating_add(data_len);
        self.file.seek(SeekFrom::Start(RIFF_SIZE_OFFSET))?;
        self.file.write_all(&riff_len.to_le_bytes())?;
        self.file.seek(SeekFrom::Start(DATA_SIZE_OFFSET))?;
        self.file.write_all(&data_len.to_le_bytes())?;
        self.file.flush()?;
        Ok(())
    }
}

fn write_header(w: &mut impl Write, sample_rate: u32, channels: u16, data_len: u32) -> Result<()> {
    let bits_per_sample: u16 = 16;
    let block_align: u16 = channels * (bits_per_sample / 8);
    let byte_rate: u32 = sample_rate * block_align as u32;

    w.write_all(b"RIFF")?;
    w.write_all(&((HEADER_BYTES - 8) + data_len).to_le_bytes())?;
    w.write_all(b"WAVE")?;

    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // fmt chunk size
    w.write_all(&1u16.to_le_bytes())?; // audio format = PCM
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&bits_per_sample.to_le_bytes())?;

    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_patched_header() {
        let path = std::env::temp_dir().join("tech-reader-wavwriter-test.wav");
        let mut w = WavWriter::create(&path, 22050, 1).expect("create");
        // 5 mono frames -> 10 data bytes.
        w.write_frames(&[0.0, 0.5, -0.5, 1.0, -1.0]).expect("write");
        w.finalize().expect("finalize");

        let bytes = std::fs::read(&path).expect("read back");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        let data_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        let riff_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(data_len, 10, "5 samples * 2 bytes");
        assert_eq!(riff_len, 36 + 10);
        assert_eq!(bytes.len(), 44 + 10);

        let _ = std::fs::remove_file(&path);
    }
}
