#!/bin/sh
set -eu
SPOTIYA_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
SPOTIYA_BASE=/data/opt/spotify-connect
SPOTIYA_BINARY=${1:-"$SPOTIYA_ROOT/spotiya"}
[ "$(id -u)" -eq 0 ] || { echo 'Run this installer as root on the speaker' >&2; exit 1; }
[ -f "$SPOTIYA_BINARY" ] || { echo "Binary not found: $SPOTIYA_BINARY" >&2; exit 1; }
"$SPOTIYA_BINARY" --version
for file in run.sh start.sh stop.sh boot.sh service.sh cli.sh S99spotify-connect; do
    [ -f "$SPOTIYA_ROOT/packaging/$file" ] || { echo "Missing packaging/$file" >&2; exit 1; }
done
if [ -e /usr/bin/spotiya ] && [ "$(readlink -f /usr/bin/spotiya)" != "$(readlink -f "$SPOTIYA_BASE/cli.sh")" ]; then
    echo '/usr/bin/spotiya belongs to another installation' >&2
    exit 1
fi
umask 077
mkdir -p "$SPOTIYA_BASE/receiver-state"
chmod 700 "$SPOTIYA_BASE/receiver-state"
backup="$SPOTIYA_BASE/backups/install-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$backup"
for file in spotiya run.sh start.sh stop.sh boot.sh service.sh cli.sh; do
    [ ! -f "$SPOTIYA_BASE/$file" ] || cp "$SPOTIYA_BASE/$file" "$backup/$file"
done
if [ -f /etc/init.d/S99spotify-connect ]; then
    cp /etc/init.d/S99spotify-connect "$backup/S99spotify-connect"
fi
cp "$SPOTIYA_BINARY" "$SPOTIYA_BASE/spotiya.new"
chmod 755 "$SPOTIYA_BASE/spotiya.new"
restore_ro=0
case ",$(awk '$2 == "/" { print $4 }' /proc/mounts)," in
    *,ro,*) mount -o remount,rw /; restore_ro=1 ;;
esac
trap 'if [ "$restore_ro" -eq 1 ]; then mount -o remount,ro /; fi' EXIT HUP INT TERM
if [ -x /etc/init.d/S99spotify-connect ]; then
    /etc/init.d/S99spotify-connect stop
elif [ -x "$SPOTIYA_BASE/stop.sh" ]; then
    "$SPOTIYA_BASE/stop.sh"
fi
mv "$SPOTIYA_BASE/spotiya.new" "$SPOTIYA_BASE/spotiya"
for file in run.sh start.sh stop.sh boot.sh service.sh cli.sh; do
    cp "$SPOTIYA_ROOT/packaging/$file" "$SPOTIYA_BASE/$file"
    chmod 755 "$SPOTIYA_BASE/$file"
done
cp "$SPOTIYA_ROOT/packaging/S99spotify-connect" /etc/init.d/S99spotify-connect
chmod 755 /etc/init.d/S99spotify-connect
ln -sf "$SPOTIYA_BASE/cli.sh" /usr/bin/spotiya
if [ "$restore_ro" -eq 1 ]; then mount -o remount,ro /; restore_ro=0; fi
trap - EXIT HUP INT TERM
/etc/init.d/S99spotify-connect start
printf 'Installed spotiya; previous files: %s\n' "$backup"
