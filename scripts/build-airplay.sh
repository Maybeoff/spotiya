#!/bin/sh
set -eu
SPOTIYA_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
python3 "$SPOTIYA_ROOT/airplay/build.py"
exec python3 "$SPOTIYA_ROOT/airplay/package.py"
