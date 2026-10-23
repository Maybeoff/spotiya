#!/bin/sh
set -eu
umask 077
SPOTIFY_DIR=/data/opt/spotify-connect
exec "$SPOTIFY_DIR/spotiya" --state-dir "$SPOTIFY_DIR/receiver-state" "$@"
