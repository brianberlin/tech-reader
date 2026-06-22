# tech-reader

Reads source code, comments, and design/spec markdown **aloud but explained** —
not read verbatim. A local Ollama model rewrites each block into spoken prose, a
local neural TTS speaks it, and a full-screen TUI shows the scrolling narration
with the current sentence highlighted. Everything runs on-device: no cloud LLM,
no cloud TTS, no telemetry.

> **Status:** Rewrite in progress. This repository was previously a TypeScript
> VS Code extension + CLI; it is being rebuilt as a single self-contained Rust
> TUI binary whose central goal is **gapless** continuous audio. See
> [`DESIGN-REWRITE.md`](DESIGN-REWRITE.md) for the full architecture spec and
> milestone plan; the prior TypeScript implementation lives in the git history.

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

Voice models are not committed. For development, place a sherpa-onnx voice bundle
under `voices/` (e.g. `vits-piper-en_US-amy-low/`). First-run download +
verification is part of milestone M6.

## License

MIT — see [`LICENSE`](LICENSE).
