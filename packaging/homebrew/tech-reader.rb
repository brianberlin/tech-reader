# Homebrew formula for tech-reader (DESIGN-REWRITE §9.5).
#
# Installs a PREBUILT binary — no compile on the user's machine, so the large
# onnxruntime static archive is never re-downloaded per user (§9.1). CI builds
# one self-contained Mach-O per arch, uploads it as a release asset, and records
# its sha256 below.
#
# Distribute via a tap (e.g. `brew tap brianberlin/tech-reader && brew install
# tech-reader`). Binaries delivered through a formula carry no com.apple.quarantine,
# so the ad-hoc-signed binary runs without a Gatekeeper prompt and needs no paid
# Developer ID or notarization (§9.4). Ship a FORMULA, never a cask.
class TechReader < Formula
  desc "Reads code, comments, and specs aloud-but-explained (local Ollama + neural TTS)"
  homepage "https://github.com/brianberlin/tech-reader"
  url "https://github.com/brianberlin/tech-reader/releases/download/v0.1.1/tech-reader-0.1.1-arm64-darwin.tar.gz"
  sha256 "18eb5ce9202734039349dba8de364a8851be6a09c133995a5ae40f36ef21cb1f"
  version "0.1.1"
  license "MIT"

  # macOS Apple Silicon is the supported target (N5). Intel (x86_64) is a
  # best-effort future addition and is not yet built/published.
  depends_on arch: :arm64

  # Ollama is a separate local daemon the user installs (never bundled, §9.3) and
  # is optional — without it tech-reader uses the offline humanizer — so it is not
  # a formula dependency; see caveats.

  def install
    bin.install "tech-reader"
  end

  def caveats
    <<~EOS
      tech-reader downloads its neural voice (~64 MB) to
        #{Dir.home}/Library/Application Support/tech-reader/voices
      on first run, verifying it against a pinned SHA-256. Every run after that
      is fully offline.

      For AI-explained narration, run a local Ollama (https://ollama.com) and
      `ollama pull llama3.2`. Without it, tech-reader falls back to a
      deterministic offline humanizer.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tech-reader --version")
  end
end
