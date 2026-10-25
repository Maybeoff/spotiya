#!/bin/sh
set -eu
SPOTIYA_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
SPOTIYA_CACHE=${SPOTIYA_CACHE:-"${XDG_CACHE_HOME:-$HOME/.cache}/spotiya"}
DECODER_PYTHON=${DECODER_PYTHON:-python3}
DECODER_TARGET=${2:-armv7}
DECODER_UNICORN=${DECODER_UNICORN:-"$SPOTIYA_CACHE/unicorn-2.1.4"}
DECODER_REVISION=8028ec436f2d9376525352dd38ed9ed6b9f6be10
DECODER_BUILD="$SPOTIYA_CACHE/playplay-$DECODER_TARGET"
if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo 'Usage: scripts/build-playplay.sh SOURCE.apk|SOURCE.so [armv7|host]' >&2
    exit 2
fi
DECODER_SOURCE=$1
case "$DECODER_TARGET" in
    host)
        DECODER_CC=${CC:-gcc}
        DECODER_STRIP=${STRIP:-strip}
        DECODER_FLAGS='-Os -ffunction-sections -fdata-sections'
        set --
        ;;
    armv7)
        SPOTIYA_TOOLCHAIN=${SPOTIYA_TOOLCHAIN:-"$SPOTIYA_CACHE/gcc-linaro-6.5.0-2018.12-x86_64_arm-linux-gnueabihf"}
        DECODER_CC="$SPOTIYA_TOOLCHAIN/bin/arm-linux-gnueabihf-gcc"
        DECODER_STRIP="$SPOTIYA_TOOLCHAIN/bin/arm-linux-gnueabihf-strip"
        if [ ! -x "$DECODER_CC" ]; then
            echo 'Run scripts/bootstrap-arm.sh first' >&2
            exit 1
        fi
        DECODER_FLAGS='-Os -mcpu=cortex-a7 -mfpu=neon-vfpv4 -ffunction-sections -fdata-sections'
        set -- -DCMAKE_SYSTEM_NAME=Linux -DCMAKE_SYSTEM_PROCESSOR=arm "-DCMAKE_C_COMPILER=$DECODER_CC"
        ;;
    *) echo 'Target must be host or armv7' >&2; exit 2 ;;
esac
mkdir -p "$DECODER_BUILD/generated" "$SPOTIYA_ROOT/dist"
"$DECODER_PYTHON" "$SPOTIYA_ROOT/decoder/prepare.py" "$DECODER_SOURCE" "$DECODER_BUILD/generated/resources.h"
if [ ! -d "$DECODER_UNICORN/.git" ]; then
    git init "$DECODER_UNICORN"
    git -C "$DECODER_UNICORN" remote add origin https://github.com/unicorn-engine/unicorn.git
    git -C "$DECODER_UNICORN" fetch --depth 1 origin "$DECODER_REVISION"
    git -C "$DECODER_UNICORN" checkout --detach FETCH_HEAD
fi
if [ "$(git -C "$DECODER_UNICORN" rev-parse HEAD)" != "$DECODER_REVISION" ] ||
    [ -n "$(git -C "$DECODER_UNICORN" status --porcelain --untracked-files=no)" ]; then
    echo 'Expected a clean pinned Unicorn 2.1.4 checkout' >&2
    exit 1
fi
cmake -S "$DECODER_UNICORN" -B "$DECODER_BUILD/unicorn" -G Ninja \
    -DCMAKE_BUILD_TYPE=MinSizeRel -DUNICORN_ARCH=x86 \
    -DUNICORN_BUILD_TESTS=OFF -DBUILD_SHARED_LIBS=OFF \
    "-DCMAKE_C_FLAGS=$DECODER_FLAGS" "$@"
cmake --build "$DECODER_BUILD/unicorn" -j "${SPOTIYA_JOBS:-4}"
# DECODER_FLAGS is a fixed compiler option list selected above.
# shellcheck disable=SC2086
"$DECODER_CC" $DECODER_FLAGS -std=c11 -Wall -Wextra -Werror \
    -I "$DECODER_UNICORN/include" -I "$DECODER_BUILD/generated" \
    "$SPOTIYA_ROOT/decoder/decoder.c" "$DECODER_BUILD/unicorn/libunicorn.a" \
    -Wl,--gc-sections -Wl,--exclude-libs,ALL -Wl,-z,noexecstack -Wl,-z,max-page-size=4096 \
    -lm -lpthread -ldl -o "$DECODER_BUILD/playplay-decoder"
"$DECODER_STRIP" "$DECODER_BUILD/playplay-decoder"
cp "$DECODER_BUILD/playplay-decoder" "$SPOTIYA_ROOT/dist/playplay-decoder-$DECODER_TARGET"
wc -c "$SPOTIYA_ROOT/dist/playplay-decoder-$DECODER_TARGET"
