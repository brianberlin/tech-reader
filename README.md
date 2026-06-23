# tech-reader

Reads source code, comments, and design/spec markdown **aloud but explained** —
not read verbatim. A local Ollama model rewrites each block into spoken prose, a
local neural TTS speaks it, and a full-screen TUI shows the scrolling narration
with the current sentence highlighted. Everything runs on-device: no cloud LLM,
no cloud TTS, no telemetry.

> **Status:** The Rust rewrite (milestones M0–M6 of
> [`DESIGN-REWRITE.md`](DESIGN-REWRITE.md)) is implemented: gapless audio spine,
> segmentation + offline/Ollama narration, look-ahead/caching/failure handling,
> the TUI with audible-sentence highlight, transport (pause/seek/speed), and
> first-run voice provisioning + packaging. This repository was previously a
> TypeScript VS Code extension + CLI; that implementation lives in the git
> history.

## Install

```sh
# Homebrew (prebuilt, no compile)
brew tap nberl-in/tech-reader && brew install tech-reader

# or curl | sh
curl -fsSL https://raw.githubusercontent.com/nberl-in/tech-reader/main/packaging/install.sh | sh
```

Both deliver one self-contained binary (onnxruntime static-linked); the neural
voice is downloaded + SHA-256-verified on first run, then everything is offline.
For AI-explained narration, run a local [Ollama](https://ollama.com) and
`ollama pull llama3.2` — without it, a deterministic offline humanizer is used.
See [`PACKAGING.md`](PACKAGING.md) for the build/release details.

## Usage

```sh
tech-reader path/to/file.rs      # narrate a source file or markdown doc
tech-reader --text path/to/file  # print the narration; no audio
tech-reader --help               # all options
```

Controls: `space` pause · `←/→` seek · `−/+` speed · `↑/↓` scroll · `f` follow ·
`q` quit.

## Why a rewrite

The old CLI synthesized and played one sentence at a time, cold-spawning a fresh
`piper` process and `afplay` per sentence — so every sentence boundary had an
audible gap. The fix is an architecture, not a faster loop: a persistent warm
synthesizer feeds a bounded look-ahead queue that feeds **one** persistently-open
audio device. Rust additionally buys in-process ONNX synthesis (via the official
`sherpa-onnx` crate, which static-links onnxruntime) and a single binary with no
external `piper` install and no runtime.

## Building

Requires the Rust toolchain (pinned to 1.96.0 via [`.tool-versions`](.tool-versions);
`asdf install` will fetch it). The first build downloads a prebuilt onnxruntime
archive (~17 MB) and static-links it — no `.dylib` to ship.

```sh
cargo build --release
```

macOS arm64 is the primary, must-work target; Linux is best-effort.

## Voices

On first run, tech-reader downloads its voice to
`~/Library/Application Support/tech-reader/voices` and verifies it against a
pinned SHA-256 ([`src/voices.rs`](src/voices.rs)); later runs are offline. For
development, point `TECH_READER_VOICE_DIR` at an already-extracted sherpa-onnx
voice bundle (e.g. `voices/vits-piper-en_US-amy-low/`) to skip the download.
Voice models are never committed.

## License

MIT — see [`LICENSE`](LICENSE).
