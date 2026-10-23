#!/bin/sh
set -eu
SPOTIYA_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
SPOTIYA_BINARY=${1:-"$SPOTIYA_ROOT/spotiya"}
[ "$(id -u)" -eq 0 ] || { echo 'Run this installer as root on the speaker' >&2; exit 1; }
exec "$SPOTIYA_BINARY" --install "$SPOTIYA_ROOT" --install-binary "$SPOTIYA_BINARY"
