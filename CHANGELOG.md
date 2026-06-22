# Changelog

## 0.1.0

Initial release.

- Read the active file, a selection, or from the cursor with **Tech Reader: Read…** commands.
- **AI mode** (default): a local Ollama model explains what code does and reads prose
  naturally — no verbatim variable names, no spoken "underscore".
- **Literal mode**: a fully offline "humanizer" expands identifiers
  (`return_item` → "return item", `getUserByID` → "get user by I D") and symbols.
- Speaks through your operating system's voices (Web Speech API) — fully offline.
- Streams narration sentence-by-sentence and queues utterances back-to-back for
  fluid, gap-free reading.
- Polished reader: sentence highlighting, teleprompter auto-scroll, progress
  scrubber, speed/volume, themes, reading fonts, sleep timer, resume, and a voice
  picker. UI adapted from markdown-read-aloud (MIT).
