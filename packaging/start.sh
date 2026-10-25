#!/bin/sh
set -eu
umask 077
# shellcheck source=packaging/service.sh
. /data/opt/spotify-connect/service.sh
if SPOTIFY_PID=$(supervisor_pid); then
    echo "Spotify Connect already running (supervisor pid $SPOTIFY_PID)"
    exit 0
fi
if receiver_pid >/dev/null; then
    "$SPOTIFY_DIR/stop.sh"
fi
rm -f /run/spotify-connect-stop
nohup setsid "$SPOTIFY_DIR/run.sh" --supervise </dev/null >/tmp/spotify-connect-supervisor.log 2>&1 &
# A cold start from slow flash can take several seconds to write the pidfile.
SPOTIFY_WAIT=0
until SPOTIFY_PID=$(supervisor_pid); do
    if [ "$SPOTIFY_WAIT" -ge 50 ]; then
        echo 'Spotify Connect supervisor failed; see /tmp/spotify-connect-supervisor.log' >&2
        exit 1
    fi
    SPOTIFY_WAIT=$((SPOTIFY_WAIT + SPOTIFY_TICK))
    pause_briefly
done
echo "Spotify Connect started (supervisor pid $SPOTIFY_PID)"
