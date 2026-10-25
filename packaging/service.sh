#!/bin/sh
SPOTIFY_DIR=/data/opt/spotify-connect
SPOTIFY_EXE=$(readlink -f "$SPOTIFY_DIR/spotiya")
SPOTIFY_PIDFILE=/run/spotify-connect.pid
SPOTIFY_SUPERVISOR_PIDFILE=/run/spotiya-supervisor.pid
# Wait loops count tenths of a second and add SPOTIFY_TICK per pause, so a BusyBox
# without usleep or fractional sleep keeps the same deadlines with one-second pauses.
# shellcheck disable=SC2034
if command -v usleep >/dev/null 2>&1; then
    SPOTIFY_TICK=1
    pause_briefly() { usleep 100000; }
elif sleep 0.1 2>/dev/null; then
    SPOTIFY_TICK=1
    pause_briefly() { sleep 0.1; }
else
    SPOTIFY_TICK=10
    pause_briefly() { sleep 1; }
fi
receiver_pid() {
    [ -f "$SPOTIFY_PIDFILE" ] || return 1
    pid=$(cat "$SPOTIFY_PIDFILE" 2>/dev/null) || return 1
    case "$pid" in ''|*[!0-9]*) return 1 ;; esac
    [ "$(readlink "/proc/$pid/exe" 2>/dev/null || true)" = "$SPOTIFY_EXE" ] || return 1
    printf '%s\n' "$pid"
}
supervisor_pid() {
    [ -f "$SPOTIFY_SUPERVISOR_PIDFILE" ] || return 1
    pid=$(cat "$SPOTIFY_SUPERVISOR_PIDFILE" 2>/dev/null) || return 1
    case "$pid" in ''|*[!0-9]*) return 1 ;; esac
    [ "$(readlink "/proc/$pid/exe" 2>/dev/null || true)" = "$SPOTIFY_EXE" ] || return 1
    tr '\000' '\n' <"/proc/$pid/cmdline" | grep -Fxq -- '--supervise' || return 1
    printf '%s\n' "$pid"
}
