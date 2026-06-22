# Notices and attributions

Tech Reader's reader webview — its layout, theming/CSS, and several UI
interactions (progress scrubbing, teleprompter auto-scroll, sleep timer,
keyboard handling) — is adapted from **markdown-read-aloud** by Robin Reiche,
used under the MIT License.

- Project: https://github.com/Robin-Reiche/markdown-read-aloud
- Copyright (c) 2026 Robin Reiche
- License: MIT

Tech Reader differs in substance: it reads source code, comments, and design
specs (not just Markdown); it explains code with a **local Ollama** model instead
of reading it verbatim; it speaks only through the OS **Web Speech API** (no
network TTS); and it streams narration sentence-by-sentence with an offline
"humanizer" that expands identifiers and symbols.
