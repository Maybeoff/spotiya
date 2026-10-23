#!/bin/sh
set -eu
SPOTIYA_BASE=/data/opt/spotify-connect
case "${1:-tui}" in
    start|stop|restart)
        exec /etc/init.d/S99spotify-connect "$1"
        ;;
    play)
        shift
        [ "$#" -gt 0 ] || { echo 'Usage: spotiya play SONG_OR_ARTIST' >&2; exit 1; }
        set -- --play "$*"
        ;;
    profile)
        shift
        if [ "${1:-}" = select ]; then
            shift
            [ "$#" -eq 1 ] || { echo 'Usage: spotiya profile select ID_OR_JSON' >&2; exit 1; }
            set -- --profile-select "$1"
        else
            [ "$#" -eq 0 ] || { echo 'Usage: spotiya profile [select ID_OR_JSON]' >&2; exit 1; }
            set -- --profile-info
        fi
        ;;
    tui|login|status|pause|resume|toggle|next|previous|volume|voice|spectrum|retry|release|version|help|profiles)
        SPOTIYA_COMMAND=${1:-tui}
        [ "$#" -eq 0 ] || shift
        set -- "--$SPOTIYA_COMMAND" "$@"
        ;;
esac
case "${1:-}" in
    --tui|--login|--play|--pause|--resume|--toggle|--next|--previous|--volume|--voice|--spectrum|--retry|--release)
        "$SPOTIYA_BASE/start.sh" >/dev/null
        ;;
esac
exec "$SPOTIYA_BASE/spotiya" "$@"
