#!/bin/sh
set -eu
# shellcheck source=packaging/service.sh
. /data/opt/spotify-connect/service.sh
touch /run/spotify-connect-stop
if SPOTIFY_PARENT=$(supervisor_pid); then
    kill -TERM "$SPOTIFY_PARENT" 2>/dev/null || true
fi
if SPOTIFY_CHILD=$(receiver_pid); then
    kill -TERM "$SPOTIFY_CHILD" 2>/dev/null || true
fi
SPOTIFY_WAIT=0
while supervisor_pid >/dev/null || receiver_pid >/dev/null; do
    if [ "$SPOTIFY_WAIT" -ge 80 ]; then
        echo 'Spotify Connect is still stopping' >&2
        exit 1
    fi
    SPOTIFY_WAIT=$((SPOTIFY_WAIT + 1))
    sleep 0.1
done
rm -f "$SPOTIFY_PIDFILE" "$SPOTIFY_SUPERVISOR_PIDFILE"
echo 'Spotify Connect stopped'
