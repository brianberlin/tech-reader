//! Voice provisioning (§9.2 / §5.4).
//!
//! Voices are too large to embed (the default is ~78 MB unpacked: a 60 MB ONNX
//! model plus 18 MB of espeak-ng phonemizer data), so the first run downloads
//! the chosen voice to a per-user data dir, verifies its **SHA-256 against a
//! pinned value**, and atomically moves it into place — never loading an
//! unverified or partial model. Every run after that is fully offline.
//!
//! Resolution order: `TECH_READER_VOICE_DIR` (an already-extracted dir, for dev)
//! → the cached voice if present and complete → download + verify + extract.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

/// How many times to (re)try a download before giving up (→ exit 3).
const MAX_ATTEMPTS: u32 = 3;

/// A downloadable voice: where to fetch it and how to verify and load it.
pub struct Voice {
    /// The archive's top-level directory name (also the on-disk voice dir name).
    pub name: &'static str,
    /// Synthesizer artifacts, relative to the voice dir.
    pub model: &'static str,
    pub tokens: &'static str,
    pub data_dir: &'static str,
    /// `.tar.bz2` download URL.
    pub url: &'static str,
    /// Expected SHA-256 of the archive (lowercase hex).
    pub sha256: &'static str,
}

/// The shipped catalog; entry 0 is the default. A quality upgrade (Kokoro-82M,
/// 24 kHz, Apache-2.0) is the planned second entry once its archive sha is
/// pinned — the loader is voice-agnostic, so adding it is data-only.
pub const CATALOG: &[Voice] = &[Voice {
    name: "vits-piper-en_US-ryan-medium",
    model: "en_US-ryan-medium.onnx",
    tokens: "tokens.txt",
    data_dir: "espeak-ng-data",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-en_US-ryan-medium.tar.bz2",
    sha256: "c546af78b6395b4e7c4ce1ed899438b64426a362f5d4ec5fecd090ded9ad7505",
}];

/// Resolved on-disk paths for a voice's three load artifacts.
pub struct VoicePaths {
    pub model: PathBuf,
    pub tokens: PathBuf,
    pub data_dir: PathBuf,
}

fn paths_in(dir: &Path, voice: &Voice) -> VoicePaths {
    VoicePaths {
        model: dir.join(voice.model),
        tokens: dir.join(voice.tokens),
        data_dir: dir.join(voice.data_dir),
    }
}

impl VoicePaths {
    fn complete(&self) -> bool {
        self.model.exists() && self.tokens.exists() && self.data_dir.exists()
    }
}

/// Resolve the default voice, downloading + verifying it on first use.
pub fn ensure_default() -> Result<VoicePaths> {
    let voice = &CATALOG[0];

    // Dev override: an already-extracted voice directory.
    if let Ok(dir) = std::env::var("TECH_READER_VOICE_DIR") {
        let paths = paths_in(Path::new(&dir), voice);
        anyhow::ensure!(
            paths.complete(),
            "TECH_READER_VOICE_DIR={dir} is missing {} / {} / {}",
            voice.model,
            voice.tokens,
            voice.data_dir
        );
        return Ok(paths);
    }

    let root = voices_root()?;
    let dir = root.join(voice.name);
    let paths = paths_in(&dir, voice);
    if paths.complete() {
        return Ok(paths);
    }
    provision(voice, &root, &dir)?;
    Ok(paths)
}

/// `<data-dir>/voices` — `~/Library/Application Support/tech-reader/voices` on
/// macOS, the XDG data dir on Linux. `TECH_READER_DATA_DIR` overrides the root.
fn voices_root() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("TECH_READER_DATA_DIR") {
        return Ok(PathBuf::from(dir).join("voices"));
    }
    let dirs = directories::ProjectDirs::from("", "", "tech-reader")
        .ok_or_else(|| anyhow!("could not determine a user data directory for voices"))?;
    Ok(dirs.data_dir().join("voices"))
}

fn provision(voice: &Voice, root: &Path, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("create voices dir {}", root.display()))?;
    eprintln!(
        "First run: downloading voice '{}' to {} (one-time, then fully offline).",
        voice.name,
        root.display()
    );
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match try_provision(voice, root, dir) {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("[voice] attempt {attempt}/{MAX_ATTEMPTS} failed: {e:#}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| anyhow!("unknown error"))
        .context(format!("could not provision voice '{}'", voice.name)))
}

fn try_provision(voice: &Voice, root: &Path, dir: &Path) -> Result<()> {
    let archive = root.join(format!("{}.tar.bz2.part", voice.name));
    let staging = root.join(format!(".staging-{}", voice.name));
    // Start each attempt clean.
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&archive);

    download_verify(voice.url, &archive, voice.sha256)?;
    extract(&archive, &staging)?;

    // The archive's top-level dir is the voice name.
    let extracted = staging.join(voice.name);
    let extracted_paths = paths_in(&extracted, voice);
    anyhow::ensure!(
        extracted_paths.complete(),
        "archive {} did not contain the expected {} layout",
        archive.display(),
        voice.name
    );

    // Atomic move into place: a rename within the same dir is atomic, so a
    // concurrent reader never sees a half-populated voice dir.
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&extracted, dir)
        .with_context(|| format!("move {} -> {}", extracted.display(), dir.display()))?;

    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&archive);
    Ok(())
}

/// Stream `url` to `dest`, hashing as we go, and fail on a SHA-256 mismatch
/// (deleting the bad file) so a partial/corrupt download is never extracted.
fn download_verify(url: &str, dest: &Path, expected_sha: &str) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .context("build HTTP client")?;
    let mut resp = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;

    let total = resp.content_length().unwrap_or(0);
    let mut file = BufWriter::new(File::create(dest).with_context(|| format!("create {}", dest.display()))?);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut downloaded = 0u64;
    let mut last_report = 0u64;
    loop {
        let n = resp.read(&mut buf).context("read response body")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("write archive")?;
        hasher.update(&buf[..n]);
        downloaded += n as u64;
        if downloaded - last_report >= 4 * 1024 * 1024 {
            last_report = downloaded;
            report_progress(downloaded, total);
        }
    }
    file.flush().context("flush archive")?;
    report_progress(downloaded, total);
    eprintln!();

    let got = hex(&hasher.finalize());
    if !got.eq_ignore_ascii_case(expected_sha) {
        let _ = std::fs::remove_file(dest);
        bail!("checksum mismatch (expected {expected_sha}, got {got})");
    }
    Ok(())
}

/// Unpack `archive` into `staging`. Uses the system `tar` (bsdtar on macOS, GNU
/// tar on Linux — both auto-detect bzip2); it is a one-time first-run setup step
/// and present on every supported platform.
fn extract(archive: &Path, staging: &Path) -> Result<()> {
    std::fs::create_dir_all(staging).with_context(|| format!("create {}", staging.display()))?;
    let status = std::process::Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(staging)
        .status()
        .context("run `tar` to unpack the voice archive")?;
    anyhow::ensure!(status.success(), "`tar` failed unpacking {}", archive.display());
    Ok(())
}

fn report_progress(done: u64, total: u64) {
    let mb = |b: u64| b as f64 / 1_048_576.0;
    if total > 0 {
        eprint!(
            "\r[voice] {:.1}/{:.1} MB ({:.0}%)    ",
            mb(done),
            mb(total),
            done as f64 / total as f64 * 100.0
        );
    } else {
        eprint!("\r[voice] {:.1} MB    ", mb(done));
    }
    let _ = std::io::stderr().flush();
}

/// Lowercase-hex encode a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        // SHA-256("abc")
        let mut h = Sha256::new();
        h.update(b"abc");
        assert_eq!(
            hex(&h.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn paths_join_under_dir() {
        let v = &CATALOG[0];
        let p = paths_in(Path::new("/voices/ryan"), v);
        assert!(p.model.ends_with("en_US-ryan-medium.onnx"));
        assert!(p.tokens.ends_with("tokens.txt"));
        assert!(p.data_dir.ends_with("espeak-ng-data"));
        assert!(!p.complete()); // nothing on disk
    }

    #[test]
    fn default_voice_sha_is_pinned_hex() {
        let s = CATALOG[0].sha256;
        assert_eq!(s.len(), 64);
        assert!(s.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn data_dir_ends_in_app_voices() {
        // §9.2: `<data>/tech-reader/voices` (Application Support on macOS).
        let dirs = directories::ProjectDirs::from("", "", "tech-reader").unwrap();
        let voices = dirs.data_dir().join("voices");
        assert!(
            voices.ends_with("tech-reader/voices"),
            "unexpected voices root: {}",
            voices.display()
        );
    }
}
