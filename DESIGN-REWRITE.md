# tech-reader — Rewrite Design Specification

**Status:** Build-ready architecture spec • **Audience:** A fresh engineer with no prior context • **Date:** 2026-06-22 • **Language: Rust — LOCKED 2026-06-22**

---

## 1. Executive Summary & Recommendation

tech-reader reads source code, comments, and design/spec markdown **aloud but explained** (not read verbatim): a local Ollama LLM rewrites each block into spoken prose, a local neural TTS speaks it, and a full-screen TUI shows the scrolling narration with the current sentence highlighted. Everything runs on-device (no cloud LLM, no cloud TTS). The product exists today as a TypeScript/Node CLI; this spec governs a rewrite whose two goals are (a) **eliminate the awkward inter-sentence audio gaps** and (b) ship a **single self-contained executable**.

### Decision: **Rust** — LOCKED 2026-06-22

**This is a committed decision, not a recommendation.** The language evaluation below (§4) and the Go/Elixir analysis are retained as the *rationale of record* for why Rust was chosen; they are no longer open for re-litigation. All implementation work (§5–§10) proceeds in Rust.

Rust wins because the hard part of this project is **gapless continuous audio**, and Rust uniquely combines three things the other candidates cannot all deliver:

1. **In-process TTS to raw PCM, no subprocess.** The official `sherpa-onnx` Rust crate (k2-fsa, v1.13.3) runs Piper/VITS *and* Kokoro ONNX models in-process and returns `&[f32]` PCM samples plus sample rate. This removes the per-sentence cold-spawn entirely.
2. **A true single static binary including ONNX inference.** `sherpa-onnx` links onnxruntime **statically by default** for CPU builds (it auto-downloads a matching prebuilt `-lib` archive at build time) and supports macOS arm64. No `.dylib` to ship alongside.
3. **No-GC, real-time-safe audio output.** `cpal`'s output callback drained from a wait-free SPSC ring buffer gives sample-accurate gapless playback with no garbage-collector jitter on the audio thread, plus a mature, actively maintained TUI (`ratatui` 0.30 + `crossterm` 0.29).

**Runner-up: Go** — lost by a hair. Go is genuinely excellent here (`ebitengine/oto` v3 is **pure-Go/no-Cgo on macOS** with a persistent `Context`+`Player` model; `bubbletea`+`bubbles/viewport` is a first-class TUI), and its audio layer is functionally equivalent to Rust's for gapless playback. It loses on the *combined* single-binary-plus-in-process-inference axis: the mature `yalue/onnxruntime_go` binding needs Cgo and **dlopens** a shared `libonnxruntime.dylib` at runtime (no single static binary). Pure-Go ONNX runtimes (`gonnx`, `onnx-go`) exist and *would* compile to one static binary, but their limited operator coverage likely won't run VITS/Kokoro, and they're ~8× slower. So the clean single-binary path realistically forces Go back to a **persistent-piper-subprocess** rather than in-process synthesis — a fine architecture, just one notch less clean than Rust's.

**Eliminated: Elixir** — lost on the single-binary axis despite the best concurrency story (GenStage/supervised Ports map beautifully onto this pipeline) and a genuinely neutralized audio worry (Membrane's PortAudio sink runs the hard real-time loop in native C; the BEAM only feeds a ringbuffer). Two newer facts narrow but do not close the gap: its idiomatic full-screen TUI stack (Ratatouille/ex_termbox) is dormant (last real commits 2021–2022), but a **maintained** replacement now exists — `ExRatatui` (binds Rust `ratatui` via Rustler NIFs, daily commits, v0.11.0 Jun 2026) — so "no maintained native TUI" is no longer true, only "less battle-tested." The decisive loss is "single binary": Burrito embeds the **entire ERTS/BEAM VM** (tens of MB, self-extracts to a per-platform app dir on first run, requires Zig 0.15.2, labeled experimental, no turnkey notarization), *and* sherpa-onnx has no first-class Elixir binding so the C API must be hand-NIF-wrapped.

> One-line reasons the others lost: **Go** — no static single binary with in-process VITS/Kokoro inference (the mature ONNX binding dlopens a dylib; pure-Go runtimes can't run these models — so ship a dylib or shell out to piper). **Elixir** — fat self-extracting BEAM bundle (Burrito) + hand-rolled sherpa NIF; the native-TUI objection is now only "young," not "unmaintained."

---

## 2. Problem Statement & Root-Cause Analysis

### 2.1 The symptom

Audio plays one sentence at a time with an audible gap between every sentence.

### 2.2 The root cause (verified against the existing source)

The current Node player (`src/cli/player.ts`, `loop()` L109–145) runs a **strictly sequential, cold-spawn-per-sentence** loop:

1. `synth(sentence)` (`src/engines/piper.ts`) cold-spawns a **fresh `piper` process** (`piper -m model -f tmpfile`), which **reloads the entire ONNX voice model**, synthesizes the **whole** sentence, writes a **temp WAV**, then reads it back.
2. `play(wav)` (L176–202) cold-spawns **`afplay`**, which **opens the CoreAudio output device**, plays, and **closes it** (resolving only on the `close` event).
3. The loop **`await`s afplay's `close` event** before starting synthesis of sentence N+1 (`idx++`, re-enter `synth`).

So synthesis and playback **never overlap**, and the gap between sentences equals: *fresh piper spawn + full ONNX model load + full-sentence inference + temp-WAV write/read + afplay spawn + CoreAudio device open*.

### 2.3 Why this design exists (and why it no longer applies)

The strict serialization was a **deliberate workaround**. The code's own header comment (`player.ts` L24–33) states that synthesizing while `afplay` runs "starves the macOS real-time audio thread and wedges CoreAudio." The repo contains two **distinct** theories for the wedge:
- (a) **CoreAudio-access failure** specific to the VS Code extension-host launch context — extension-host children land in a context lacking a CoreAudio session, worked around elsewhere via `launchctl asuser` (`playerPanel.ts` L266–270).
- (b) **real-time-thread starvation** from running CPU-heavy synthesis concurrently with playback (`reader.js` L254–257) — **independent of launch context**.

**Honest reframe (this is load-bearing — a verification *refuted* the naive claim):** Leaving the VS Code extension host removes theory (a), but it does **not** by itself make naive overlap safe — the project's own native CLI (`src/cli/player.ts` L24–33) **still serializes** for theory (b). The fix is therefore **not** "just run synth and play concurrently." The fix is an **architecture**: a *persistent warm synthesizer* + a *bounded look-ahead queue* + **one persistent audio sink** fed a continuous PCM stream, so the audio device is opened **once** for the whole session and the CPU-heavy synthesis writes *ahead* into a buffer that the audio callback merely drains. That topology both eliminates the gap and structurally avoids the per-sentence device thrash that triggered the original wedge.

### 2.4 The explicit reframe: the gap fix is an architecture change, required in any language

The inter-sentence gap is **not** caused by Node's concurrency model (Node's `spawn()` is async/non-blocking and can overlap synth and playback), and "better concurrency" is **not** the real justification for the rewrite. The same gapless result is achievable *in the existing Node/TS code* — with caveats: a warm persistent Piper requires the **legacy `rhasspy/piper` binary, the piper HTTP server, or the Python `PiperVoice` API** (the newer `piper1-gpl` CLI reloads the model per invocation), and on macOS the persistent sink cannot be `afplay` (it can't read raw PCM from stdin) — you'd need `sox play`, `ffplay`, or the `speaker` npm bindings. The mandatory, language-independent change is:

> **Replace the sequential cold-spawn loop with a producer/consumer pipeline: a persistent warm TTS feeds a bounded look-ahead synthesis queue that feeds ONE persistent, continuously-fed audio output stream.**

**What the rewrite *additionally* buys** (the legitimate reason to leave Node):

- **A single self-contained binary** with no external `piper` install and no Node runtime.
- **In-process synthesis** (Rust + sherpa-onnx) so there is no subprocess at all — the cleanest possible version of the warm-synthesizer pattern, with a native streaming callback delivering PCM as it is generated.
- A **maintained, ergonomic TUI** layer replacing the hand-rolled raw-ANSI TUI.

---

## 3. Requirements

### 3.1 Functional

- **F1.** Accept a file path (source code, comments, or markdown/spec) as input.
- **F2.** **Block segmentation** — split the document into typed blocks (heading / paragraph / listItem / quote / code / comment / table) with source line ranges. Two scanners: a markdown line-scanner and a comment-aware source-code chunker.
- **F3.** **AI narration** — stream each block through local Ollama (`http://localhost:11434`), which rewrites it into spoken prose (code explained, tables summarized, prose smoothed) using spoken-prose prompt templates. Pull **complete sentences** out of the token stream so speech can start before a block finishes.
- **F4.** **Offline humanizer fallback** — a deterministic, rule-based rewrite used when Ollama is unreachable, so the app still speaks.
- **F5.** **TTS** — speak each sentence via a local neural engine, **gaplessly**.
- **F6.** **TUI** — full-screen scrolling narration with the **current sentence highlighted** and auto-scrolled into view; controls: play/pause, prev/next sentence, speed up/down, scroll, quit.
- **F7.** **Transport semantics** — pause/resume (instant, device stays open), prev/next sentence (seek), speed change (pitch-preserving), cancel/quit (clean teardown).

### 3.2 Non-functional

- **N1. Gapless audio.** No audible cold-spawn gap between sentences. Inter-sentence pause becomes a *tunable* (default **120 ms** of intentional silence; see §6.5), not a variable artifact.
- **N2. Single binary.** One self-contained executable; no Node/VM runtime; no separately-installed `piper`. Voice models may be downloaded on first run to a cache dir (see §9).
- **N3. Offline / local-first / privacy-preserving.** All inference on-device. No cloud LLM, no cloud TTS, no telemetry. Ollama is the only external local process and is a user-installed dependency.
- **N4. TUI parity.** Match or exceed the current TUI's features (highlight, scroll-follow, transport).
- **N5. Cross-platform posture.** **macOS arm64 is the primary, must-work target.** Linux is **best-effort for v1** (the chosen stack supports it, but `cpal` needs `libasound2-dev` + a C compiler at build time, complicating the static-binary story there). Windows out of scope for v1.
- **N6. Startup latency.** Time-to-first-audio bounded by one block's first-sentence synthesis, not by a process spawn.
- **N7. Bounded memory.** All inter-stage queues bounded; PCM cache capped by total bytes (see §6.4 for the length_scale interaction).

---

## 4. Language Evaluation Matrix

Scores are 1–10, higher is better. "Fit" is the weighted overall for *this* project (gapless audio + single binary weighted heaviest).

| Dimension (weight) | **Rust** | **Go** | **Elixir** |
|---|---|---|---|
| Concurrency fit (15%) | 8 — tokio tasks + bounded mpsc + `CancellationToken`; CPU work on `spawn_blocking`. Clean, slightly verbose. | 9 — goroutines + buffered channels + `context.Context`; the canonical pipeline language. | 10 — GenStage/supervised Ports + "let it crash" supervision; best-in-class for this exact shape (a plain GenServer+bounded-queue is the idiomatic-weight fit; GenStage is defensible). |
| Audio maturity / gapless (25%) | 9 — `cpal` callback + wait-free `rtrb` ring; **no GC jitter on the audio thread**, sample-accurate. (Edge over Go is real but **less decisive** for latency-tolerant buffered TTS than for hard-real-time audio.) | 8 — `oto` v3 persistent `Context`+`Player` from an `io.Reader`, **no Cgo on macOS**; gapless via one continuous reader. Sub-ms GC, present but neutralized by buffering. | 7 — Membrane PortAudio sink: RT callback runs in **native C** reading a lock-free ringbuffer; BEAM only feeds it (per-process GC, no global STW). Gapless achievable but conditional on the feeder staying ahead (default 4096-frame slack); PortAudio is a native dep. |
| TUI (15%) | 9 — `ratatui` 0.30 + `crossterm` 0.29, de-facto standard, stable policy, ideal List/highlight/scroll. | 9 — `bubbletea` + `bubbles/viewport` (SetHighlights/EnsureVisible), vibrant ecosystem. | 6 — idiomatic Ratatouille/ex_termbox **dormant since 2021–2022**; `ExRatatui` (binds Rust ratatui via Rustler) is **maintained** (daily commits, v0.11 Jun 2026) but young. Raised from prior estimate: "maintained replacement" now exists. |
| Piper/ONNX integration (15%) | 10 — **official `sherpa-onnx` crate**, in-process VITS/Piper+Kokoro, returns `&[f32]` PCM, static-by-default. | 6 — in-process needs Cgo + dlopened `libonnxruntime.dylib` (`yalue/onnxruntime_go`); pure-Go runtimes can't run VITS/Kokoro; clean path is **persistent piper subprocess**. | 6 — no first-class binding; Ortex (Rust `ort` NIF, pre-1.0) can run ONNX but needs an espeak-ng front-end (FFI to libespeak-ng, not a rewrite); pragmatic path is **persistent piper Port**. |
| Single-binary ease (20%) | 9 — `cargo build --release` → one file; sherpa-onnx static-links onnxruntime (CPU/macOS). ~30–80 MB; build-time download once. | 6 — pure-Go static binary **only if shelling out to piper**; the mature in-process binding ships a sidecar dylib. ~8–20 MB. | 3 — Burrito embeds whole ERTS/BEAM (tens of MB), self-extracts on first run, requires Zig 0.15.2, experimental, no turnkey notarization. |
| Dev velocity (10%) | 6 — steepest curve (borrow checker, async lifetimes, real-time-safe-audio discipline); highest correctness ceiling. | 9 — fast to write, simple concurrency, fast compiles. | 8 — pleasant pattern-matching/pipe-operator core; friction at audio/packaging/NIF edges. |
| **Weighted fit** | **8.4** | **7.3** | **5.8** |

**Fairness note on Elixir:** its concurrency model is *the best* for this pipeline and its audio worry is genuinely neutralized (the hard real-time work happens in PortAudio's C thread, not the BEAM; underrun yields silence-padding, not a crash). It loses primarily on a small cleanly-notarizable single binary; the native-TUI concern is now downgraded from "unmaintained" to "young."

**Fairness note on Go:** Go's *audio* stack (oto v3) is no-Cgo on macOS and a genuine peer to cpal/rodio for gapless playback — the audio layer alone does **not** favor Rust. Go loses only on the *combination* of single-binary + in-process VITS/Kokoro inference.

---

## 5. Recommended Architecture (Rust)

### 5.1 Component / pipeline diagram

```
                          tech-reader (single Rust binary)
 ┌──────────────────────────────────────────────────────────────────────────────┐
 │                                                                                │
 │   file path                                                                    │
 │      │                                                                         │
 │      ▼                                                                         │
 │  ┌───────────────┐   blocks (typed, line-ranged)                              │
 │  │  SEGMENTER    │───────────────────────────────┐                            │
 │  │ md scanner +  │  (pure, sync)                 │                            │
 │  │ code chunker  │                               ▼                            │
 │  └───────────────┘                     ┌────────────────────┐                 │
 │                                        │  NARRATOR (tokio)   │                 │
 │                                        │ reqwest NDJSON      │                 │
 │   Ollama :11434  ◀── HTTP stream ──────│ stream → sentence   │                 │
 │   (or OFFLINE                          │ segmenter           │                 │
 │    HUMANIZER if down)                  └─────────┬──────────┘                 │
 │                                        sentences │ bounded mpsc(16)            │
 │                                                  ▼                             │
 │                                        ┌────────────────────┐                 │
 │                                        │  SYNTH WORKER       │  PCM cache      │
 │                                        │ spawn_blocking:     │◀── (model,      │
 │                                        │ sherpa-onnx         │     len_scale,  │
 │                                        │ OfflineTts.generate │     text)→PCM   │
 │                                        └─────────┬──────────┘                 │
 │                              Vec<f32> PCM (sentence-keyed) │ bounded mpsc(2–3) │
 │                                                  ▼                             │
 │                                        ┌────────────────────┐                 │
 │                                        │  AUDIO FEEDER       │                 │
 │                                        │ push f32 → ring buf │                 │
 │                                        │ + boundary table    │                 │
 │                                        └─────────┬──────────┘                 │
 │                                  rtrb SPSC ring  │                             │
 │                                                  ▼                             │
 │                                        ┌────────────────────┐   CoreAudio /    │
 │                                        │ cpal OUTPUT CALLBACK│──▶ ALSA device   │
 │                                        │ (RT thread, drain   │   (opened ONCE)  │
 │                                        │  ring → &mut[f32])  │                  │
 │                                        └────────────────────┘                 │
 │                                                  ▲                             │
 │   ┌────────────────────┐  control (pause/seek/   │  audible-sentence-index     │
 │   │   TUI (ratatui)    │  speed/quit) via         │  (from frames consumed)     │
 │   │ render loop +       │  watch / CancellationToken                            │
 │   │ key events          │◀─────────────────────────────────────────────────────┤
 │   └────────────────────┘  state: sentences, current idx, scroll, play/pause    │
 └──────────────────────────────────────────────────────────────────────────────┘
```

### 5.2 Concurrency design (Rust specifics)

A small set of long-lived tasks connected by **bounded** channels (back-pressure everywhere):

- **Runtime:** `tokio` multi-thread for I/O + orchestration. The only truly async work is the Ollama HTTP stream; everything else is CPU-bound or real-time.
- **Segmenter:** pure synchronous functions, called up front (or per-block as blocks stream). No task of its own.
- **Narrator task (tokio):** owns the `reqwest` streaming response to Ollama, parses NDJSON line-by-line, runs the **sentence streamer** (`unicode-segmentation` for sentence boundaries), and emits complete sentences onto `mpsc::channel::<Sentence>(16)`. On connection failure it switches to the **offline humanizer**, which produces the same `Sentence` stream synchronously.
- **Synth worker:** pulls a `Sentence`, checks the PCM cache, else calls `sherpa-onnx` `OfflineTts::generate(...)` **on `tokio::task::spawn_blocking`** (inference is blocking CPU). Copies the returned `&[f32]` to an owned `Vec<f32>` (the slice borrows a C-owned buffer — you **must** `.to_vec()`), tags it with its sentence index and sample count, populates the cache, and sends it onto `mpsc::channel::<SynthPcm>(LOOKAHEAD)`. **The bounded send is the primary back-pressure valve**: the worker blocks (does not synthesize ahead) when the look-ahead queue is full.
- **Audio feeder:** a dedicated thread that drains the `SynthPcm` channel, pushes f32 samples into the **`rtrb` SPSC ring buffer** (producer side), inserts the configured inter-sentence silence, and maintains a **boundary table** mapping cumulative frame offsets → sentence index.
- **cpal output callback (OS real-time thread):** does **nothing but drain the ring buffer** into `&mut [f32]` (the consumer side of `rtrb`). **No allocation, no locks, no syscalls, no `println!`, no blocking in this callback** — on underrun, write silence (and bump an `AtomicU64` underrun counter for out-of-callback logging; see §5.4). It also publishes the running count of frames consumed (an `AtomicU64`) so the TUI can compute the currently-audible sentence index from the boundary table.
- **TUI task (tokio + ratatui):** renders at a fixed frame cadence from shared state (`Arc<Mutex<AppState>>` for non-realtime state), reads key events via `crossterm`, and sends control commands.

**Look-ahead depth:** **2–3 sentences of PCM** (start with 2; 3 for margin on long sentences / Ollama bursts). This is enough to fully mask synthesis latency because sherpa-onnx VITS/Kokoro run faster than real-time on Apple Silicon CPU. Bound it by count; bound the *cache* by total bytes.

**Device sample rate (do not assume 22050):** open the cpal output stream at the **active voice's** sample rate, read at startup from the voice's `.onnx.json` (`audio.sample_rate`) — Kokoro ≈ 24000 Hz, Piper medium/high 22050 Hz, Piper low/x_low 16000 Hz. All ring-buffer/silence/look-ahead durations below are expressed in **milliseconds** and converted to sample counts at *that* rate. If the chosen device cannot open at the voice rate, resample once in the feeder to the device rate; never hardcode a constant.

**Ring buffer sizing:** target **~150–300 ms** of audio in the `rtrb` ring (e.g. at 22050 Hz mono f32, 300 ms ≈ 6615 samples; at 24000 Hz ≈ 7200). Big enough to absorb scheduler jitter, small enough that seek latency stays low.

**Click avoidance at boundaries:** sherpa-onnx sentences begin/end near zero amplitude, so plain concatenation is usually click-free. For safety apply a **2–5 ms linear ramp** at each join (or snap to a zero-crossing). Do **not** insert long silence to hide clicks; insert the *intentional* inter-sentence silence (default 120 ms, §6.5) as zero samples for natural prosody — this is a tunable, not an artifact.

### 5.3 Transport (pause / seek / speed / cancel)

- **Pause — callback-gating, NOT ring-draining (decided).** On pause, the cpal callback writes silence to `&mut [f32]` while **not** draining the ring buffer and **not** advancing the frames-consumed `AtomicU64`. A `paused: AtomicBool` (relaxed load, single branch, RT-safe) gates the callback. The feeder also stops draining `SynthPcm`, so synthesis naturally back-pressures and parks. This avoids two bugs the drain approach causes: (1) losing ~150–300 ms of already-synthesized buffered audio, and (2) corrupting the highlight index by advancing the audible-frame counter during silence. **Resume is instant** (clear the flag); the cpal stream is never re-opened.
- **Seek (prev/next/jump):** (1) signal the synth worker via a `watch` channel / `CancellationToken` to abandon in-flight work and reseat its sentence cursor at the target index; (2) **ramp the ring to silence (2–5 ms) before clearing it** and clear the `SynthPcm` channel (avoids a seek click); (3) reset the frames-consumed counter and rebuild the boundary table from the new index; (4) refill look-ahead from the new index. The **PCM cache makes seek-back instant**.
- **Speed change:** see §6.4 — re-spawn synthesis config with a new `length_scale`, ramp+flush the (tiny) look-ahead, resynthesize from the current sentence. Cheap because look-ahead is only 2–3 sentences.
- **Cancel/quit:** signal cancellation, drop the synth handle, stop the cpal stream, restore the terminal (leave alternate screen, disable raw mode), exit 0.

### 5.4 Failure handling (per stage)

| Failure | Behavior |
|---|---|
| **Ollama unreachable / drops mid-stream** | Narrator catches the connection/idle-timeout error, logs once, and switches the **offline humanizer** in as the `Sentence` source for the remainder (or until a later block reconnects). User hears uninterrupted narration; a status line notes "offline humanizer." |
| **Voice model download** | Download to a temp file in the cache dir, verify the **expected SHA-256** (shipped in a small embedded manifest), then **atomic-rename** into place. On checksum mismatch or partial download: delete temp, retry up to N times, then fail with a clear message and exit code 3. Never load an unverified/partial `.onnx`. |
| **`sherpa-onnx generate()` error** (bad model, OOM, malformed text) | The `spawn_blocking` returns `Result`; on `Err` the synth worker logs the sentence index + error, **skips that sentence** (emits a short silence so the index stays aligned), and continues. Three consecutive synth failures abort with exit code 4. |
| **cpal device disconnect / default-device change / format change** | The cpal error callback signals the orchestrator; tear down the stream, **re-query the default output device**, reopen at the voice rate (or resample), rebuild the ring, and resume from the current frame counter. If reopen fails after retries, exit code 5 with a device message. |
| **Ring underrun** | Callback writes silence and increments an `AtomicU64` underrun counter; a low-priority task logs the rate out-of-band. Sustained underruns → log a hint to raise look-ahead/ring size (never block or allocate in-callback). |
| **Ollama present but model missing / 404** | Treated as "unreachable": fall back to humanizer, surface the model name to install. |

**CLI exit codes:** `0` clean quit · `1` usage/bad-args · `2` input file unreadable · `3` voice-download/verify failure · `4` synthesis failure · `5` audio-device failure. Errors print to stderr; the TUI is torn down first so messages aren't swallowed by the alternate screen.

---

## 6. TTS Engine Decision

### 6.1 Engine: **sherpa-onnx runtime, default voice Kokoro-82M, Piper/VITS as the low-latency alternative**

Decouple **runtime** from **model**:

- **Runtime: `sherpa-onnx` (Apache-2.0).** It loads VITS/Piper, Kokoro, and Matcha models behind one `OfflineTts` API, runs fully offline on onnxruntime, and exposes raw f32 PCM. Using its Apache-2.0 runtime to *load* a model sidesteps the GPL-3.0 `piper1-gpl` engine.
- **Default voice (see §6.6 for the spec default vs. quality default): Kokoro-82M** (Apache-2.0 weights, ~24 kHz) is the **quality** option — higher quality than typical Piper voices (TTS-Arena consensus, not a controlled MOS), runs **faster than real-time on Apple Silicon CPU** (community reports ~5×–14× RT on M1-class), and its single blanket Apache-2.0 license avoids the **per-voice license-checking burden** of Piper voices (MIT / CC0 / CC-BY-attribution / CC-BY-NC-SA / dataset-restricted). Caveat: Kokoro covers fewer languages (8) than Piper (30+).
- **Alternative / spec-default voice: a small Piper/VITS ONNX voice** for lowest latency / smallest footprint and zero-network first run. Loaded through the same sherpa-onnx API. Read each voice's actual sample rate from its `.onnx.json` (`audio.sample_rate`) at startup — **do not hardcode 22050**; low/x_low Piper voices are 16000 Hz and would mis-pitch (play ~1.38× too fast).

### 6.2 Integration mode: **in-process ONNX (not a subprocess)**

Use the **official `sherpa-onnx` Rust crate** (k2-fsa, v1.13.3; published by the k2-fsa lead maintainer csukuangfj). **Do not use the third-party `sherpa-rs` binding — it was archived/deprecated 2026-06-06 and its README redirects to the official upstream Rust API.**

API shape (verified — note the corrections vs. early research):
- `OfflineTts`, `OfflineTtsConfig`, `OfflineTtsVitsModelConfig` (VITS = Piper's architecture); Kokoro config also supported.
- `tts.generate(...)` returns a `GeneratedAudio` whose fields are **private** — access PCM via **methods**: `audio.samples() -> &[f32]` (a **borrowed** slice over a C-owned buffer; `.to_vec()` to keep it) and `audio.sample_rate() -> i32`.
- A **streaming callback** `|samples: &[f32], progress: f32| -> bool` delivers PCM **as the model generates**. Granularity caveat: for VITS/Piper the unit is **per generated segment (typically per sentence)** — it streams chunks *before the whole input text finishes*, **not** true sub-word mid-sentence partials. That is more than enough for gapless multi-sentence playback; first-audio latency is bounded by the first sentence's full synthesis.
- The crate is FFI bindings to the C++ core; its build script downloads/links native libs on first build (relevant if CI is offline/locked-down — see §9.1).

### 6.3 Why in-process beats the subprocess for gaplessness

Synthesizing in-process to a `Vec<f32>` that feeds the ring buffer removes every per-sentence cost in the old gap path: no process spawn, no model reload, no temp WAV, no `afplay` spawn, no per-sentence device open. The synthesizer simply runs ahead into the bounded look-ahead queue while the cpal callback drains the ring.

**Documented fallback (keep in reserve):** a **persistent warm `piper --output-raw` subprocess** (model loaded once; reads one **line** per stdin line — internally split into sentences; streams flushed S16LE mono PCM to stdout). This is the proven low-risk path if onnxruntime bundling proves painful, and is exactly the integration Go would use. Two quirks to honor: (1) `--output-raw` is a **single concatenated PCM stream with no in-band sentence delimiter**, so feed **one sentence per line** and track boundaries **out-of-band** by counting the bytes returned for that line; (2) synthesis is **per-sentence-atomic** (a line's audio appears only after that line finishes), so first-audio latency is bounded by the first sentence — the same bound as in-process. Use the **legacy `rhasspy/piper` binary, the HTTP server, or Python `PiperVoice`** for true model persistence; the `piper1-gpl` CLI reloads per invocation.

### 6.4 Speed control: **`length_scale` (pitch-preserving), with re-synthesis**

The old `afplay -r` resamples and **shifts pitch** (chipmunk effect) — wrong. Use the model's **`length_scale`** (sherpa-onnx VITS config; settable **per-call**, no model-JSON edit; `<1` faster, `>1` slower, `1.0` normal): it scales phoneme durations fed to the VITS neural decoder and **re-synthesizes**, preserving pitch with natural re-predicted prosody. Because it requires re-synthesis (you cannot re-pace already-rendered PCM), a speed change flushes the **2–3-sentence** look-ahead and resynthesizes from the current sentence — **cheap** precisely because look-ahead is shallow. (Caveat: "cheap" assumes desktop-class CPU where RTF ≈ 0.2; on weak hardware RTF can approach/exceed 1.0. On the macOS arm64 target this is a non-issue.) A speed change incurs at most one look-ahead-buffer's worth of synthesis latency before new-rate audio plays. Reserve real-time WSOLA/phase-vocoder time-stretch only if instantaneous mid-utterance speed change is ever required; it is not for v1.

**TUI speed ladder (F6).** Discrete steps mapping to `length_scale` (inverse of perceived speed multiplier):

| Step (×speed) | 0.75× | 1.0× | 1.25× | 1.5× | 1.75× | 2.0× |
|---|---|---|---|---|---|---|
| `length_scale` | 1.333 | 1.000 | 0.800 | 0.667 | 0.571 | 0.500 |

Up/down arrows move one step; default is 1.0×.

**Cache-vs-speed interaction (closes the byte-cap gap):** the PCM cache key is `(model, length_scale, normalized_text)`, so each distinct speed would otherwise multiply memory against the byte cap. **Decision: cache only the active `length_scale`.** On a speed change, **clear the PCM cache** (the look-ahead is flushed anyway) and repopulate at the new scale. This keeps total cached bytes bounded by a single speed's worth and makes the byte cap meaningful. (Seek-back within the *current* speed stays instant; seek-back across a just-changed speed re-synthesizes, which is acceptable and rare.)

### 6.5 Inter-sentence silence

Default **120 ms** of intentional zero-sample silence between sentences (spec default; user-adjustable in the TUI within ~60–250 ms). This is prosodic spacing, *not* gap-hiding; clicks are handled by the boundary ramp (§5.2).

### 6.6 Voice default (spec decision, confirm/override)

**Spec default: a small embedded Piper voice** (zero-network first run, lowest latency, smallest footprint), with **Kokoro offered as the quality upgrade** downloaded on first selection. Rationale: guarantees M1/M6 acceptance on a clean machine with no network. Override to ship Kokoro as default if first-run quality is prioritized over zero-network startup (see Open Question Q1).

---

## 7. Port Plan

### 7.1 Re-implement (pure, dependency-free logic — port the *behavior*, not the old code)

1. **Block segmentation.** Two scanners producing typed, line-ranged blocks (`heading | paragraph | listItem | quote | code | comment | table`): a **markdown line-scanner** and a **comment-aware source-code chunker** that splits code into code/comment blocks. Mechanical port to pure Rust functions over `&str` with line tracking.
2. **Sentence streaming.** Pull **complete sentences** out of the Ollama token stream so speech starts before a block finishes. The TS version uses `Intl.Segmenter`; replace with the **`unicode-segmentation`** crate (sentence/word boundaries). Maintain a small buffer that emits a sentence as soon as a terminal boundary is seen.
3. **Ollama NDJSON streaming client.** Re-implement the custom client behaviors: stream the response body, split on newlines, deserialize each JSON object (`serde_json`), with **idle-timeout**, **error classification**, and **offline detection**. Use `reqwest` (`stream` feature) for parity and full control over timeouts/errors (prefer it over `ollama-rs`, which abstracts those away).
4. **Spoken-prose prompt templates.** Port the prompt templates that force spoken-prose output (code explained, tables summarized, prose smoothed). Plain string templates per block type.
5. **Offline humanizer fallback.** Port the deterministic, rule-based rewrite used when Ollama is down, producing the same `Sentence` stream so the rest of the pipeline is unchanged.
6. **PCM-by-text cache.** Carry forward the cache, but key it on **(model, length_scale, normalized text)** and store **raw f32/PCM** (directly enqueuable, no WAV header parsing). **Cap by total bytes**, not entry count (~44 KB/s per 22050 Hz mono s16le; budget a few tens of MB), and cache **only the active speed** (§6.4).

### 7.2 Redesign (the one part that must change)

7. **The sequential cold-spawn player → the pipelined gapless audio engine.** Replace `player.ts`'s synthesize-to-WAV → afplay → wait-for-exit → repeat loop with the **persistent warm synthesizer + bounded look-ahead queue + single persistent cpal output stream** of §5. This is the entire point of the rewrite; everything else is a mechanical port.

---

## 8. Concrete Library Choices (Rust)

| Concern | Crate | One-line justification |
|---|---|---|
| Async runtime / orchestration | **`tokio`** (multi-thread) | The only async work is the Ollama HTTP stream; tokio gives tasks, bounded `mpsc`, `spawn_blocking` for inference. |
| Cancellation / transport | **`tokio-util`** (`CancellationToken`) + **`tokio::sync::watch`** | Clean seek/pause/cancel signaling to synth + feeder. |
| HTTP / Ollama | **`reqwest`** (`stream`) + **`serde_json`** | Byte-stream NDJSON line-by-line with full timeout/error control, mirroring the existing custom client. |
| Sentence/word segmentation | **`unicode-segmentation`** | Direct replacement for `Intl.Segmenter`. |
| TTS runtime + models | **`sherpa-onnx`** (official k2-fsa crate, v1.13.x) | In-process VITS/Piper + Kokoro → `&[f32]` PCM; static onnxruntime by default; macOS supported. |
| Audio output | **`cpal`** | Lowest-layer persistent output stream; RT callback fillable from a ring buffer; abstracts CoreAudio/ALSA; no GC. |
| Lock-free ring buffer | **`rtrb`** | **Wait-free** SPSC ring explicitly built for audio (preferred over `ringbuf`/`HeapRb`, which is lock-free but not advertised wait-free; cpal's own example uses `HeapRb`, so it's a viable fallback). |
| TUI framework | **`ratatui` 0.30** | De-facto standard, maintained, stable-API policy; `List`+`ListState`+`highlight_style`+`scroll_padding` map exactly onto highlighted-current-sentence + auto-scroll. (0.30 itself shipped breaking changes — workspace modularization, MSRV 1.85 — so the line is newly settled; pin it.) |
| Terminal backend | **`crossterm` 0.29** | Cross-platform raw mode, key events, alternate screen; ratatui's default backend. Slow cadence (0.29 latest >1yr) reflects stability, not abandonment. |
| Asset embedding / download | **`rust-embed`** (small default voice) + a tiny SHA-256-verifying downloader | Embed one tiny Piper voice; fetch large voices (Kokoro) to a cache dir on first run with checksum + atomic rename. |

> Avoid in the audio callback: any allocation, `Mutex` lock, `println!`, file I/O, or blocking call. The callback must be a pure `rtrb` drain plus relaxed atomic loads/stores.

---

## 9. Single-Binary Build & Distribution Plan

### 9.1 Build

- `cargo build --release` (add `strip = true` + `lto = true` in `[profile.release]`).
- **onnxruntime linking:** `sherpa-onnx` **links statically by default** for CPU builds. When `SHERPA_ONNX_LIB_DIR` is unset, its build script **auto-downloads a matching prebuilt `-lib` archive** (onnxruntime bundled) from GitHub releases and static-links it → **one self-contained binary, no `.dylib` to ship**. Static linking is **CPU-only** (GPU EPs would force shared linking — irrelevant on macOS, which uses CPU/CoreML).
- **Reproducible/hermetic build contract:** the *first* build needs network and is **not** hermetic. To make CI deterministic:
  - **Pin** the sherpa-onnx crate version (`=1.13.3`) **and** the onnxruntime `-lib` archive: set `SHERPA_ONNX_LIB_DIR` to a pre-fetched, **SHA-256-pinned** archive checked into CI cache (record the expected sha256 in the repo, e.g. `build/onnxruntime-osx-arm64.sha256`).
  - **CI cache step:** restore the `-lib` archive from cache keyed by `{crate-version}-{target}-{sha256}`; on miss, download once, verify sha256, populate cache. Subsequent builds are offline and reproducible.
- **Binary size:** expect **~30–80 MB** because static onnxruntime is large. Acceptable for the single-binary goal.

### 9.2 Voice models (the `.onnx` + `.onnx.json`)

- **Download-on-first-run** to a cache dir (`~/Library/Application Support/tech-reader/voices` on macOS; XDG cache on Linux), with **SHA-256 verify + atomic rename** (§5.4). Voices are 10–60 MB (Kokoro ONNX ~327 MB) and users may want several or none — **do not `include_bytes!` large voices**.
- **Embed one tiny default Piper voice** with `rust-embed` for true zero-network first run, extracted to the cache dir on first launch (spec default, §6.6).

### 9.3 Ollama

- **User-installed system dependency** (a separate local daemon on `:11434`). **Never bundle.** Detect on startup; if absent/model-missing, instruct the user and fall back to the offline humanizer for narration.

### 9.4 macOS signing / Gatekeeper / notarization

- **Apple Silicon requires at least an ad-hoc signature** to execute arm64 code — but the standard Rust/clang toolchain **applies this automatically**, so it imposes **no Developer-ID or notarization cost**.
- **Gatekeeper's notarization enforcement only fires on files carrying `com.apple.quarantine`.** Binaries delivered via **Homebrew formula / curl / tar** generally carry **no** quarantine attribute and run **unsigned without prompts** → **no paid Apple Developer ID or notarization required** for these channels.
- **Caveats to honor:** quarantine can attach/propagate if the artifact is downloaded by a Gatekeeper-aware app (browser) or copied by a store that preserves xattrs. If you ever ship a **browser-downloadable `.dmg`/`.zip`**, you must either notarize or document `xattr -d com.apple.quarantine`. Homebrew **casks** (GUI bundles) are quarantined and subject to the Sept 2026 crackdown — **ship a formula, not a cask.**

### 9.5 Distribution channel & artifact contract (decided)

- **Channel: a Homebrew formula that installs a prebuilt bottle.** Do **not** build from source on the user's machine (that re-downloads the onnxruntime `-lib` per user and needs a full Rust toolchain). Instead:
  - CI builds the release binary for `arm64-darwin` (and best-effort `x86_64-darwin`), produces a **bottle tarball** containing the single binary, and records its **sha256** in the formula's `bottle do` block.
  - The formula's `url`/`sha256` (for the source fallback) and the bottle `sha256` are both pinned. Users get the **prebuilt bottle** by default — one verified binary, no compile, no onnxruntime re-download.
  - **Artifact contract:** one Mach-O executable `tech-reader`, ad-hoc signed by the toolchain, no bundled `.dylib`, no quarantine (formula channel). First run downloads + verifies the chosen voice into the cache dir.
- **Also offer `curl | sh`** fetching the same bottle tarball (no signing needed). A browser-downloadable archive is **out of scope** for v1 (would need notarization or an `xattr` instruction).

### 9.6 How the native terminal binary sidesteps the old CoreAudio problem

The original strict-sequential workaround targeted a wedge from the **VS Code extension-host launch context** (children lacking a CoreAudio session) **and** real-time-thread starvation from concurrent synth+playback. A native terminal binary launched from a normal GUI/login session has full CoreAudio access, removing the first cause. The **architecture** removes the second: the device is opened **once** via `cpal`, the RT callback only **drains a ring buffer** (it never runs synthesis), and CPU-heavy inference happens on separate `spawn_blocking` threads writing *ahead* into that buffer — so playback is never starved by synthesis and the device is never thrashed open/closed per sentence.

---

## 10. Phased Implementation Milestones

Each milestone is independently testable. **De-risk the audio spine first** (it carries the new binding *and* the gapless claim). Spec defaults from §6.6 (small Piper default voice), §6.5 (120 ms pause), and N5 (Linux best-effort) unblock M0/M6 acceptance without waiting on Open Questions.

- **M0 — Audio spine prototype (de-risk first).** Hardcode a few sentences. Wire `sherpa-onnx` `OfflineTts::generate` (on `spawn_blocking`) → `Vec<f32>` → `rtrb` ring → `cpal` output callback, opening the device at the voice's actual sample rate. **Acceptance:** continuous, click-free playback of several concatenated sentences with the 120 ms inter-sentence silence and **no cold-spawn gap**. Confirms both the binding and gaplessness before anything else is built. *(Keep the persistent-piper-subprocess fallback ready if onnxruntime bundling blocks here.)*
- **M1 — Segmentation + offline narration walking skeleton.** Port block segmentation + the **offline humanizer** + sentence streaming. Feed humanizer sentences straight into the M0 spine. **Acceptance:** point at a markdown/code file and hear it narrated gaplessly, fully offline, no Ollama.
- **M2 — Ollama streaming narration.** Add the `reqwest` NDJSON client + spoken-prose prompts + sentence streamer; fall back to the humanizer on failure. **Acceptance:** Ollama-explained narration starts before a block finishes; killing Ollama mid-run degrades gracefully to the humanizer.
- **M3 — Look-ahead, back-pressure, caching, failure handling.** Introduce the bounded `Sentence` and `SynthPcm` channels (depth 2–3), the PCM cache keyed by (model, length_scale, text) capped by bytes, and the §5.4 failure paths (synth-error skip, underrun counter, exit codes). **Acceptance:** memory stays bounded under a long file; a stalled Ollama drains cleanly (silence, not clicks) and resumes; a forced synth error skips one sentence without desyncing the index.
- **M4 — TUI.** `ratatui` scrolling narration, current-sentence highlight driven by the frames-consumed `AtomicU64` + boundary table, auto-scroll, key handling. **Acceptance:** highlight tracks the audible sentence; scroll-follow works.
- **M5 — Transport.** Play/pause (**callback-gating**, device stays open, counter does not advance), prev/next seek (ramp+flush ring + channel, reseat cursor, rebuild boundary table, cache-accelerated), speed via `length_scale` re-synth with the §6.4 ladder + cache-clear, quit/teardown (terminal restored). **Acceptance:** all controls correct; pause→resume is instant and the highlight does not drift; seek-back is instant from cache; speed change is pitch-preserving and memory stays single-speed-bounded.
- **M6 — Gapless polish & packaging.** Tune ring size, look-ahead depth, inter-sentence silence, boundary ramps; choose voices; wire first-run voice download (SHA-256 + atomic rename); CI bottle build + Homebrew formula. **Acceptance:** no audible artifacts across a long document; `brew install` yields a single working prebuilt binary on a clean Mac with no compile and no onnxruntime re-download.

---

## 11. Risks & Mitigations

| Risk | Severity | Mitigation |
|---|---|---|
| **onnxruntime bundling/linking + binary bloat** (build-time download, 30–80 MB) | High | Prototype M0 first to confirm the static build on macOS; pin the crate version **and** the `-lib` archive sha256; cache the archive in CI (§9.1); accept the size as the cost of single-binary in-process inference. |
| **Binding-layer churn** (official `sherpa-onnx` crate is newer/less battle-tested; `sherpa-rs` was just archived) | Med | Pin a known-good version; keep the **persistent-piper-subprocess fallback** behind a feature flag as a proven escape hatch. |
| **Real-time-safe audio discipline** (any alloc/lock/blocking in the cpal callback → glitches) | High | Callback is a pure `rtrb` drain + relaxed atomics; allocate buffers ahead; lint/review the callback; load-test under CPU pressure; underrun counter surfaces regressions. |
| **Click/pop on seek or boundary** | Med | 2–5 ms ramps / zero-crossing snaps at joins; on seek, ramp ring to silence before clearing. |
| **Speed-change re-synth latency on weak hardware** | Low (macOS target is fast) | Shallow look-ahead keeps re-synth small; document that aggressive speed toggling on low-end CPUs costs a short pause; cache-clear-on-speed-change keeps memory bounded. |
| **Steeper Rust curve slows delivery** | Med | Mechanical ports (segmentation/prompts/humanizer) are low-risk; concentrate effort/review on the audio spine and transport state machine. |
| **Voice licensing** (Piper voices vary: CC-BY/CC-BY-NC-SA/dataset-restricted) | Low | Default to a permissively-licensed embedded Piper voice; offer **Kokoro (blanket Apache-2.0)** as the quality upgrade; for any Piper voice shipped/downloaded, surface its MODEL_CARD license. |
| **Device sample-rate mismatch** (hardcoding 22050 mis-pitches 16 kHz/24 kHz voices) | Med | Read `audio.sample_rate` from the voice `.onnx.json`; open cpal at that rate or resample once in the feeder; never hardcode. |
| **Ollama absent / offline** | Low | Detect on startup; offline humanizer keeps the app fully functional. |

---

## 12. Open Questions for the User (spec defaults already chosen — confirm or override)

1. **Voices: embed vs download / default engine.** **Spec default:** embed one small Piper voice (zero-network first run), Kokoro as a downloadable quality upgrade. Override to make **Kokoro** the shipped default (better quality, needs one network fetch on first run)?
2. **Linux support level.** **Spec default:** macOS arm64 primary, **Linux best-effort** (cpal needs `libasound2-dev` + a C compiler at build time, complicating the static-binary story). Promote Linux to a must-work v1 target?
3. **Speed-control approach.** **Spec default:** `length_scale` re-synthesis (pitch-preserving, tiny re-synth, ladder in §6.4). Confirm, or do you need instantaneous mid-utterance speed change (would require WSOLA time-stretch DSP)?
4. **Inter-sentence pause length.** **Spec default:** 120 ms, user-adjustable 60–250 ms in the TUI. Override the default or the adjustability?
5. **Distribution channel.** **Spec default:** Homebrew formula installing a **prebuilt bottle** + `curl | sh` of the same tarball (no signing). Also want a browser-downloadable archive (then needs notarization or an `xattr` instruction)?
6. **Fallback policy.** If onnxruntime static linking proves painful on a target, is shipping the **persistent-piper-subprocess** mode (requires a `piper` binary present) an acceptable fallback, or must everything stay in-process?

---

## 13. Appendix: Key Verified Facts (with sources)

- **Root cause = architecture, not Node.** `src/cli/player.ts` `loop()` (L109–145) synthesizes a full WAV (fresh `piper` per sentence via `src/engines/piper.ts`, model reloaded), then runs `afplay` to exit (L176–202), then synthesizes the next — never overlapping; the header comment (L24–33) says overlap "starves the macOS real-time audio thread and wedges CoreAudio." *Verified against repo source + Node `child_process` docs.* **Corrected claim:** leaving VS Code alone does **not** make naive overlap safe — the native CLI still serializes for the starvation reason; the **architecture** (persistent synth + look-ahead + one persistent sink) is the actual fix. The two repo theories are distinct: extension-host CoreAudio-access (`playerPanel.ts` L266–270, fixed via `launchctl asuser`) vs. launch-context-independent RT-thread starvation (`reader.js` L254–257).
- **In-Node gaplessness is achievable but caveated.** A warm persistent Piper needs the **legacy `rhasspy/piper` binary, the HTTP server, or Python `PiperVoice`** (the `piper1-gpl` CLI reloads the model per invocation); the macOS sink can't be `afplay` (no raw-PCM stdin) — use `sox play`/`ffplay`/`speaker`. 1-deep look-ahead is plain async scheduling. *Source: repo + piper1-gpl CLI docs + rhasspy/piper #136.*
- **Official `sherpa-onnx` Rust crate** (k2-fsa, v1.13.3, by csukuangfj) does offline VITS/Piper + Kokoro TTS in-process; PCM via **methods** `audio.samples() -> &[f32]` (borrowed; `.to_vec()` to keep) and `audio.sample_rate() -> i32`; plus a streaming callback `|&[f32], f32| -> bool`. Streaming granularity is **per generated segment (≈per sentence)** for VITS/Piper, not sub-word. Crate is FFI to the C++ core; build script downloads native libs on first build. *Source: crates.io / k2-fsa repo `rust/.../src/tts.rs` (GeneratedAudio at L443, `samples()` L449, `sample_rate()` L464), `vits_tts.rs`.*
- **`sherpa-onnx` links onnxruntime statically by default** (CPU), auto-downloading prebuilt `-lib` archives when `SHERPA_ONNX_LIB_DIR` is unset; macOS x86_64+arm64 supported → single self-contained binary. Static is CPU-only (GPU EPs force shared); `shared` is a non-default opt-out feature. *Source: docs.rs/sherpa-onnx v1.13.3, GH Discussion #1202.*
- **`sherpa-rs` (third-party) archived/deprecated 2026-06-06**, README redirects to upstream k2-fsa/sherpa-onnx's official Rust API. *Source: GitHub API (archived=true, updated_at 2026-06-06) + README.*
- **`cpal` callback + lock-free ring buffer** is the standard gapless pattern; **`rtrb` is the wait-free SPSC** crate recommended for audio over `ringbuf`/`HeapRb` (which cpal's own `feedback.rs` example uses — viable but not advertised wait-free). `rodio`'s `Sink::append()` is the lower-effort path but has a documented small gap between appended sources (#219), which is why a manual ring buffer is used for true gaplessness. Rust's no-GC audio thread is a genuine advantage, though for *buffered* TTS (latency-tolerant) Go's sub-ms GC and BEAM's per-process GC are also viable. *Source: docs.rs/cpal, mgeier/rtrb, rodio #219, Discord Go→Rust migration.*
- **`ratatui` 0.30 (Dec 2025) + `crossterm` 0.29 (Apr 2025)** are mature/maintained with a stated stability policy (≥2-version deprecation; slow-evolving `ratatui-core`); `List`+`ListState`+`highlight_style`+`scroll_padding` fit highlighted-current-sentence + auto-scroll. 0.30 itself shipped breaking changes (workspace modularization, MSRV 1.85) so the line is newly settled; patch releases continued into Jun 2026. crossterm 0.29 stable >1yr (maturity, not abandonment). *Source: ratatui releases/docs, crossterm docs.rs.*
- **`ebitengine/oto` v3 (Go)** is **no-Cgo on macOS** (auto-links AudioToolbox via purego), one persistent `Context` (single device; multiple contexts unsupported), Players each from one un-shared `io.Reader`, supports `FormatSignedInt16LE` (one of three formats, not mandated). A genuine peer to cpal/rodio for gapless playback. **But** in-process VITS/Kokoro ONNX in Go needs Cgo + a shipped `libonnxruntime.dylib` (`yalue/onnxruntime_go` dlopens it); pure-Go runtimes (`gonnx`/`onnx-go`) can't run these models and are ~8× slower → clean single binary requires shelling out to piper. *Source: oto README/pkg.go.dev; yalue/onnxruntime_go; gonnx/onnx-go.*
- **Piper persistent-process facts** (for the subprocess fallback): the `piper1-gpl` CLI reads **stdin line-by-line** (a line, not a sentence — internally split into sentences), loads the ONNX model **once per process**, and `--output-raw` streams flushed **S16LE mono PCM** to stdout; the stream has **no in-band sentence delimiter** (track boundaries out-of-band, one sentence per line); inter-chunk `--sentence-silence` is not a parseable marker; sample rate comes from the voice `.onnx.json` `audio.sample_rate` (**22050 medium/high, 16000 low/x_low** — not a constant); synthesis is **per-sentence-atomic** (a line's audio appears only after that line finishes). The CLI's per-invocation slowness refers to repeated process spawns, not reloads within one persistent process. *Source: verified against installed `piper-tts` 1.4.2 `__main__.py` + OHF-Voice/piper1-gpl source + voice `.onnx.json` examples.*
- **`length_scale`** changes pace **without pitch shift** (re-predicts VITS durations → re-synthesizes), settable **per-call** (not a JSON edit); speed changes require re-synthesis (cheap with shallow look-ahead; RTF≈0.2 desktop, can approach/exceed 1.0 on weak HW). *Source: piper1-gpl API/CLI docs, VITS paper (arXiv 2106.06103).*
- **Kokoro-82M** ships **Apache-2.0 weights** (base + onnx-community ONNX export), runs **faster than real-time on Apple Silicon CPU** via ONNX (~5×–14× RT, M1-class, CPU not GPU), judged higher-quality than typical Piper voices (TTS-Arena/consensus, not a controlled MOS), covers fewer languages (8 vs Piper 30+). **rhasspy/piper (MIT) archived Oct 6 2025**; active dev is **OHF-Voice/piper1-gpl (GPL-3.0, v1.4.2 Apr 2026)**; Piper voices carry **varying per-voice licenses** (MIT/CC0/CC-BY/CC-BY-NC-SA/dataset-restricted) — loading via Apache-2.0 sherpa-onnx + Kokoro avoids both the GPL engine and per-voice attribution. (Caveat: if a pipeline still phonemizes via espeak-ng, that is GPL; sherpa-onnx vendors espeak-ng C source.) *Source: HF model cards, piper discussion #271, repo archival banners.*
- **macOS distribution:** Gatekeeper notarization fires only on `com.apple.quarantine`-tagged files; Homebrew-formula/curl/tar delivery carries no quarantine → **unsigned CLI runs without prompts, no Developer ID needed**. Quarantine can still propagate via browser downloads or xattr-preserving stores (pnpm #11056). Apple Silicon requires only an **ad-hoc signature**, applied automatically by the toolchain. **Casks** (not formulae) are quarantined and subject to the Sept 2026 crackdown. *Source: eclecticlight.co, MITRE T1553.001, Homebrew docs/#20755.*
- **Burrito (Elixir)** bundles the **entire ERTS/BEAM VM** into a self-extracting binary (tens of MB) that **unpacks to a per-platform app dir on first run** (not in-place), requires **Zig 0.15.2**, is labeled **experimental**, and has **no turnkey macOS notarization** — the basis for eliminating Elixir on the single-binary axis. *Source: burrito README/hexdocs.*
- **Elixir TUI status (updated):** Ratatouille/ex_termbox **dormant** (last real commits 2021-10-17 / 2022-02-14, not archived), but **`ExRatatui`** (mcass19, binds Rust ratatui via Rustler NIFs, v0.11.0 Jun 2026, daily commits, precompiled NIFs) is a **maintained** replacement — so the prior "no maintained native TUI" basis is corrected to "maintained but young." *Source: GitHub API + ElixirForum.*
- **Elixir audio not disqualifying:** `Membrane.PortAudio.Sink` runs PortAudio in **callback mode** on its **native C real-time thread** reading a lock-free ringbuffer; the BEAM only feeds it via a demand-driven NIF (default 4096-frame slack). Underrun → C-side silence-padding (`memset`), not a crash. Gapless is conditional on the feeder staying ahead. *Source: membrane_portaudio_plugin source + hexdocs.*