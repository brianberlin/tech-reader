# Tech Reader

**Listen to code, comments, and design specs — read aloud and actually explained.**

Tech Reader turns the file you're looking at into clear spoken English. Source code is
*explained* by a model running on your own machine instead of being read character by
character, so you never hear `return_item` pronounced as “return underscore item.” Speech
comes from your operating system's built-in voices.

Everything runs locally. Your code is sent to a [local Ollama](https://ollama.com) server on
`localhost` and nowhere else; the text-to-speech uses the OS Web Speech API. **No API keys,
no sign-up, nothing leaves your machine.**

## How it works

1. You run a **Tech Reader: Read…** command on a file, selection, or from the cursor.
2. Tech Reader splits the document into blocks (functions, comments, Markdown sections).
3. Each block is narrated at the right **altitude** for its content:
   - **Code** → explained (purpose, parameters and their types, what each branch returns) — not
     read line-by-line, and interpolation like `#{name}` is described, not spelled out.
   - **Tables / dense structured data** → distilled to the key takeaway in a sentence or two,
     instead of reading every cell.
   - **Prose** (Markdown, comments, design specs) → read naturally and faithfully, with
     identifiers and markup smoothed for the ear.
   - **Headings** → spoken directly.

   This happens in **AI mode** (default, via the local Ollama model, streamed sentence by
   sentence). In **Literal mode** a deterministic, offline *humanizer* does the identifier work
   (`getUserByID` → “get user by I D”, `MAX_LEN` → “max length”, `a && b` → “a and b”).
4. The reader speaks the narration with your chosen OS voice, queuing sentences back-to-back
   for fluid, gap-free reading, and highlights each sentence as it's spoken.

If Ollama isn't running or the model isn't installed, Tech Reader automatically falls back to
the offline humanizer and tells you so — it always works.

## Setup

1. **Install Ollama** from <https://ollama.com> and start it (`ollama serve`, or just launch
   the app).
2. **Pull a model** — a small instruct model is plenty:
   ```sh
   ollama pull llama3.2
   # or, tuned for code:
   ollama pull qwen2.5-coder
   ```
3. Set `techReader.ollama.model` if you chose a different model.

No Ollama? Set `techReader.mode` to `literal` (or just use it — it falls back automatically).

## Commands

| Command | Default keybinding |
| --- | --- |
| Tech Reader: Read File | — (also the speaker icon in the editor title bar) |
| Tech Reader: Read Selection | right-click a selection |
| Tech Reader: Read From Cursor | `Ctrl/Cmd+Alt+R` |
| Tech Reader: Play / Pause | `Ctrl/Cmd+Alt+Space` |
| Tech Reader: Stop | `Esc` in the reader |
| Tech Reader: Toggle AI / Literal Mode | — |

In the reader: **Space** play/pause, **←/→** previous/next sentence, **+/-** speed,
**M** mute, **F** cycle font, **Alt+Click** a sentence to jump to its source line.

## Settings

All under `techReader.*` — notably `mode`, `proseHandling`, `codeHandling`, `tables`
(`summarize` / `read` / `skip`), `ollama.baseUrl`, `ollama.model`, `ollama.temperature`,
`ollama.idleTimeoutMs`, `speed`, `voiceURI`, and `dictionary` (your own abbreviation
expansions for literal mode).

## Develop / run from source

```sh
npm install
npm run build        # or: npm run watch
```

Press **F5** (“Run Tech Reader”) to launch an Extension Development Host, open any file, and
run **Tech Reader: Read File**.

## Credits

The reader UI is adapted from [markdown-read-aloud](https://github.com/Robin-Reiche/markdown-read-aloud)
by Robin Reiche (MIT). See [NOTICE.md](NOTICE.md). Tech Reader is MIT licensed.
