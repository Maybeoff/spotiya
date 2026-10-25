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
SPOTIFY_KILLED=0
while supervisor_pid >/dev/null || receiver_pid >/dev/null; do
    if [ "$SPOTIFY_WAIT" -ge 100 ]; then
        echo 'Spotify Connect is still stopping' >&2
        exit 1
    elif [ "$SPOTIFY_WAIT" -ge 80 ] && [ "$SPOTIFY_KILLED" -eq 0 ]; then
        # A receiver stuck in ALSA ignores SIGTERM; without this only a reboot recovers.
        echo 'Spotify Connect did not stop in 8 seconds; killing it' >&2
        if SPOTIFY_PARENT=$(supervisor_pid); then
            kill -KILL "$SPOTIFY_PARENT" 2>/dev/null || true
        fi
        if SPOTIFY_CHILD=$(receiver_pid); then
            kill -KILL "$SPOTIFY_CHILD" 2>/dev/null || true
        fi
        SPOTIFY_KILLED=1
    fi
    SPOTIFY_WAIT=$((SPOTIFY_WAIT + SPOTIFY_TICK))
    pause_briefly
done
rm -f "$SPOTIFY_PIDFILE" "$SPOTIFY_SUPERVISOR_PIDFILE"
echo 'Spotify Connect stopped'
