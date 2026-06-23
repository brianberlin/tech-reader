#!/bin/sh
# tech-reader installer (DESIGN-REWRITE §9.5): fetches the prebuilt, self-contained
# binary for this Mac and installs it. No compile, no onnxruntime download, no
# Node. The neural voice is fetched + verified on first run.
#
#   curl -fsSL https://raw.githubusercontent.com/nberl-in/tech-reader/main/packaging/install.sh | sh
#
# Override the version with TECH_READER_VERSION, the install dir with PREFIX.
set -eu

REPO="nberl-in/tech-reader"
VERSION="${TECH_READER_VERSION:-latest}"
PREFIX="${PREFIX:-/usr/local/bin}"

os="$(uname -s)"
[ "$os" = "Darwin" ] || { echo "tech-reader: only macOS is supported by this installer (Linux: build from source)."; exit 1; }

case "$(uname -m)" in
  arm64)  arch="arm64-darwin" ;;
  x86_64) arch="x86_64-darwin" ;;
  *) echo "tech-reader: unsupported architecture $(uname -m)"; exit 1 ;;
esac

if [ "$VERSION" = "latest" ]; then
  base="https://github.com/$REPO/releases/latest/download"
  # Resolve the tag so the filename (which embeds the version) is correct.
  VERSION="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's:.*/tag/v::')"
fi
name="tech-reader-${VERSION}-${arch}"
url="https://github.com/$REPO/releases/download/v${VERSION}/${name}.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "tech-reader: downloading $name ..."
curl -fsSL "$url" -o "$tmp/t.tar.gz"
tar -C "$tmp" -xzf "$tmp/t.tar.gz"

mkdir -p "$PREFIX"
install -m 0755 "$tmp/$name/tech-reader" "$PREFIX/tech-reader"
echo "tech-reader: installed to $PREFIX/tech-reader"
echo "Run 'tech-reader --help' to get started. First run downloads the voice (~64 MB)."
