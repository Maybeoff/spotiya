#!/bin/sh
set -eu
SPOTIYA_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$SPOTIYA_ROOT"
binary=${1:-"${CARGO_TARGET_DIR:-target}/armv7-unknown-linux-gnueabihf/release/spotiya"}
[ -f "$binary" ] || { echo "Binary not found: $binary" >&2; exit 1; }
mkdir -p dist
stage=$(mktemp -d "$SPOTIYA_ROOT/dist/package.XXXXXX")
trap 'rm -rf "$stage"' EXIT HUP INT TERM
mkdir -p "$stage/spotiya/packaging" "$stage/spotiya/scripts" "$stage/spotiya/bin"
mkdir -p "$stage/spotiya/profiles"
cp profiles/*.json "$stage/spotiya/profiles/"
cp "$binary" "$stage/spotiya/spotiya"
cp README.md LICENSE "$stage/spotiya/"
cp packaging/* "$stage/spotiya/packaging/"
cp scripts/install.sh "$stage/spotiya/scripts/"
cp bin/spotiya "$stage/spotiya/bin/"
tar -czf dist/spotiya-armv7-linux-gnueabihf.tar.gz -C "$stage" spotiya
cp install.sh dist/install.sh
cd dist
sha256sum install.sh > INSTALL_SHA256SUMS
sha256sum spotiya-armv7-linux-gnueabihf.tar.gz > SHA256SUMS
