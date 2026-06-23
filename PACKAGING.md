# Packaging & distribution

Implements DESIGN-REWRITE §9. The goal: one self-contained executable a user
installs with **no compile and no onnxruntime re-download**, that runs unsigned
without Gatekeeper prompts.

## Build

```sh
cargo build --release            # adds strip + lto + codegen-units=1 (Cargo.toml)
```

- **onnxruntime is static-linked** into the binary by default for CPU builds
  (§9.1). When `SHERPA_ONNX_LIB_DIR` is unset, `sherpa-onnx`'s build script
  downloads a matching prebuilt archive once and links it in — so the release
  binary has **no `.dylib` to ship**. Expect a 30–80 MB binary; that is the cost
  of in-process inference in one file.
- The **first** build needs network (to fetch the onnxruntime archive) and is
  not hermetic. CI makes it reproducible by caching that archive keyed by crate
  version + target (`.github/workflows/release.yml`), recording its sha256 into
  `build/onnxruntime-<arch>.sha256`. Commit that file to pin the archive.

## Artifact contract (§9.5)

- One Mach-O executable `tech-reader`, **ad-hoc signed by the toolchain** (the
  standard Rust/clang toolchain does this automatically on Apple Silicon — no
  Developer ID, no notarization cost, §9.4).
- No bundled `.dylib`. The release workflow asserts this with `otool -L`.
- Voice models are **not** in the binary. On first run tech-reader downloads the
  chosen voice (~64 MB) to `~/Library/Application Support/tech-reader/voices`,
  verifies it against a pinned SHA-256, and atomically moves it into place
  (`src/voices.rs`). Every later run is fully offline.

## Distribution channels

Both deliver the **same prebuilt tarball** — no source build on the user's
machine (that would re-download the onnxruntime archive per user).

1. **Homebrew formula** (`packaging/homebrew/tech-reader.rb`) — a binary formula
   in a tap:
   ```sh
   brew tap nberl-in/tech-reader && brew install tech-reader
   ```
   Ship a **formula, not a cask**: formula/curl/tar delivery carries no
   `com.apple.quarantine`, so the unsigned binary runs without a prompt. Casks
   are quarantined and subject to the Sept-2026 crackdown (§9.4).
2. **`curl | sh`** (`packaging/install.sh`) — fetches the same release tarball
   for the user's arch and installs it to `/usr/local/bin`.

A browser-downloadable `.dmg`/`.zip` is **out of scope for v1**: a
Gatekeeper-aware browser attaches `com.apple.quarantine`, which would then
require notarization or a documented `xattr -d com.apple.quarantine` step (§9.4).

## Releasing

1. Bump `version` in `Cargo.toml` and `packaging/homebrew/tech-reader.rb`.
2. Tag: `git tag v0.1.0 && git push --tags`.
3. CI (`release.yml`) builds both arches, attaches `tech-reader-<ver>-<arch>.tar.gz`
   to the GitHub release, and prints each tarball's **sha256** (also surfaced as
   a workflow notice).
4. Paste those sha256 into the formula's `on_arm`/`on_intel` blocks; commit.

## Ollama (§9.3)

Never bundled — a separate local daemon the user installs
(`brew install ollama`, optional in the formula). Detected on startup; if absent
or the model is missing, tech-reader falls back to the offline humanizer.

## Acceptance (M6)

- No audible artifacts across a long document (gapless spine + cubic resampler +
  boundary ramps, verified through M0–M5).
- `brew install` / `curl | sh` yields a single working binary on a clean Mac with
  **no compile and no onnxruntime re-download**; first run fetches + verifies the
  voice, then runs offline.
