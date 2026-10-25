#!/bin/sh
set -eu
umask 077
SPOTIYA_AIRPLAY_SOURCE=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
SPOTIYA_BASE=/data/opt/spotify-connect
[ "$(id -u)" = 0 ] || { echo 'Run as root on the station' >&2; exit 1; }
[ -x "$SPOTIYA_BASE/spotiya" ] || { echo 'Install Spotiya first' >&2; exit 1; }
"$SPOTIYA_BASE/spotiya" --help | grep -q -- '--airplay on/off' || {
    echo 'Install Spotiya 0.9.0 or newer first' >&2; exit 1;
}
cd "$SPOTIYA_AIRPLAY_SOURCE/airplay"
sha256sum -c SHA256SUMS
./bin/shairport-sync -V | grep -q AirPlay2
./bin/nqptp -V
./bin/avahi-daemon --version
./bin/dbus-daemon --version >/dev/null
exec 9>"$SPOTIYA_BASE/receiver-state/install.lock"
flock -n 9 || { echo 'Another installation is running' >&2; exit 1; }
SPOTIYA_STAGE=$(mktemp -d "$SPOTIYA_BASE/.airplay-install.XXXXXX")
SPOTIYA_BACKUP=
SPOTIYA_RUNNING=0
if [ -r /run/spotify-connect.pid ]; then
    SPOTIYA_PID=$(cat /run/spotify-connect.pid)
    if [ "$(readlink "/proc/$SPOTIYA_PID/exe")" = "$(readlink -f "$SPOTIYA_BASE/spotiya")" ]; then SPOTIYA_RUNNING=1; fi
fi
cleanup() { rm -rf "$SPOTIYA_STAGE"; }
trap cleanup EXIT HUP INT TERM
cp -R . "$SPOTIYA_STAGE/"
chmod 755 "$SPOTIYA_STAGE/bin/"*
if [ "$SPOTIYA_RUNNING" = 1 ]; then "$SPOTIYA_BASE/stop.sh" 9>&-; fi
if [ -e "$SPOTIYA_BASE/airplay" ]; then
    SPOTIYA_BACKUP="$SPOTIYA_BASE/.airplay-backup-$(date +%Y%m%d-%H%M%S)-$$"
    mv "$SPOTIYA_BASE/airplay" "$SPOTIYA_BACKUP"
fi
if ! mv "$SPOTIYA_STAGE" "$SPOTIYA_BASE/airplay"; then
    if [ -n "$SPOTIYA_BACKUP" ]; then mv "$SPOTIYA_BACKUP" "$SPOTIYA_BASE/airplay"; fi
    if [ "$SPOTIYA_RUNNING" = 1 ]; then "$SPOTIYA_BASE/start.sh" 9>&-; fi
    exit 1
fi
if [ "$SPOTIYA_RUNNING" = 1 ] && ! "$SPOTIYA_BASE/start.sh" 9>&-; then
    "$SPOTIYA_BASE/stop.sh" 9>&- || true
    mv "$SPOTIYA_BASE/airplay" "$SPOTIYA_STAGE"
    if [ -n "$SPOTIYA_BACKUP" ]; then mv "$SPOTIYA_BACKUP" "$SPOTIYA_BASE/airplay"; fi
    "$SPOTIYA_BASE/start.sh" 9>&- || true
    echo 'Receiver start failed; previous AirPlay package restored' >&2
    exit 1
fi
echo 'AirPlay 2 installed. Enable with: spotiya airplay on'
