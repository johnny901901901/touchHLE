#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
REPO=$(CDPATH= cd -- "$ROOT/../.." && pwd)
TARGET=${1:-aarch64-apple-ios-sim}
CONFIGURATION=${2:-Debug}

# Which emulator core to build. This repository holds HyperHLE; point
# TOUCHHLE_CORE_REPO at a checkout of another core (an upstream touchHLE tree,
# say) to build that one instead. Its dylib is copied in beside HyperHLE's so
# the app can ship both and load whichever a game is set to use.
CORE_REPO=${TOUCHHLE_CORE_REPO:-"$REPO"}
CORE_REPO=$(CDPATH= cd -- "$CORE_REPO" && pwd)
CORE_ENTRY="$CORE_REPO/platform/ios/rust-entry/Cargo.toml"
if [ ! -f "$CORE_ENTRY" ]; then
    echo "No iOS entry crate in $CORE_REPO (expected $CORE_ENTRY)" >&2
    exit 1
fi

case "$TARGET" in
    aarch64-apple-ios-sim|aarch64-apple-ios)
        ;;
    *)
        echo "Usage: $0 [aarch64-apple-ios-sim|aarch64-apple-ios]" >&2
        exit 2
        ;;
esac

case "$CONFIGURATION" in
    Debug|Release)
        ;;
    *)
        echo "Usage: $0 [aarch64-apple-ios-sim|aarch64-apple-ios] [Debug|Release]" >&2
        exit 2
        ;;
esac

CARGO_TARGET_DIR="$CORE_REPO/build/rust-ios-native"
TOUCHHLE_BOOST_ROOT=${TOUCHHLE_BOOST_ROOT:-"$REPO/vendor/boost"}
CMAKE_TOOLCHAIN_FILE="$ROOT/cmake/TouchHLEiOS.cmake"
CMAKE_GENERATOR=Ninja
CMAKE="$ROOT/scripts/cmake-ios.sh"
DEVELOPER_DIR=${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}
IPHONEOS_DEPLOYMENT_TARGET=14.7
CFLAGS="${CFLAGS:-} -ffile-prefix-map=$HOME=/build -fdebug-prefix-map=$HOME=/build"
CXXFLAGS="${CXXFLAGS:-} -DFMT_CONSTEVAL= -ffile-prefix-map=$HOME=/build -fdebug-prefix-map=$HOME=/build"

# The core is a dylib that links against the shared SDL2 embedded in the app
# bundle (see build-sdl-shared.sh), not against a private static copy.
case "$TARGET" in
    aarch64-apple-ios) SDL_SDK=iphoneos ;;
    *) SDL_SDK=iphonesimulator ;;
esac
SDL_LIB_DIR="$REPO/build/sdl-shared-$SDL_SDK/install/lib"
if [ ! -f "$SDL_LIB_DIR/libSDL2-2.0.0.dylib" ]; then
    echo "Shared SDL2 was not found at $SDL_LIB_DIR" >&2
    echo "Run platform/ios/scripts/build-sdl-shared.sh $SDL_SDK first." >&2
    exit 1
fi
SDL_LINK_ARGS="-C link-arg=-L$SDL_LIB_DIR -C link-arg=-Wl,-rpath,@executable_path/Frameworks -C link-arg=-Wl,-rpath,@loader_path"

# The install name has to be set by the linker, not by install_name_tool
# afterwards: patching it shifts the LINKEDIT string pool, and if that lands on
# a 4-byte rather than 8-byte boundary dyld on the device refuses to load the
# dylib with "mis-aligned LINKEDIT string pool".
CORE_LIB_NAME=$(awk '
    /^\[lib\]/ { in_lib = 1; next }
    /^\[/ { in_lib = 0 }
    in_lib && /^name[[:space:]]*=/ {
        gsub(/^name[[:space:]]*=[[:space:]]*"|"[[:space:]]*$/, "")
        print
        exit
    }
' "$CORE_ENTRY")
if [ -z "$CORE_LIB_NAME" ]; then
    echo "Could not read the [lib] name from $CORE_ENTRY" >&2
    exit 1
fi
CORE_DYLIB="lib$CORE_LIB_NAME.dylib"
INSTALL_NAME_ARGS="-C link-arg=-Wl,-install_name,@rpath/$CORE_DYLIB"

IOS_LINK_ARGS="$SDL_LINK_ARGS $INSTALL_NAME_ARGS --remap-path-prefix=$HOME=/build -C link-arg=-framework -C link-arg=AVFoundation -C link-arg=-framework -C link-arg=AudioToolbox -C link-arg=-framework -C link-arg=CoreBluetooth -C link-arg=-framework -C link-arg=CoreGraphics -C link-arg=-framework -C link-arg=CoreHaptics -C link-arg=-framework -C link-arg=CoreMotion -C link-arg=-framework -C link-arg=Foundation -C link-arg=-framework -C link-arg=GameController -C link-arg=-framework -C link-arg=Metal -C link-arg=-framework -C link-arg=OpenGLES -C link-arg=-framework -C link-arg=QuartzCore -C link-arg=-framework -C link-arg=UIKit"

for command in cargo cmake ninja xcrun; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "Missing required command: $command" >&2
        exit 1
    fi
done

if [ ! -d "$TOUCHHLE_BOOST_ROOT/boost" ]; then
    echo "Boost headers were not found at $TOUCHHLE_BOOST_ROOT" >&2
    echo "Extract Boost into vendor/boost or set TOUCHHLE_BOOST_ROOT." >&2
    exit 1
fi

case "$TARGET" in
    aarch64-apple-ios-sim)
        SDKROOT=$(xcrun --sdk iphonesimulator --show-sdk-path)
        CARGO_TARGET_AARCH64_APPLE_IOS_SIM_RUSTFLAGS="$IOS_LINK_ARGS"
        export SDKROOT CARGO_TARGET_AARCH64_APPLE_IOS_SIM_RUSTFLAGS
        ;;
    aarch64-apple-ios)
        SDKROOT=$(xcrun --sdk iphoneos --show-sdk-path)
        CARGO_TARGET_AARCH64_APPLE_IOS_RUSTFLAGS="$IOS_LINK_ARGS"
        export SDKROOT CARGO_TARGET_AARCH64_APPLE_IOS_RUSTFLAGS
        ;;
esac

export CARGO_TARGET_DIR TOUCHHLE_BOOST_ROOT
export CMAKE_TOOLCHAIN_FILE CMAKE_GENERATOR CMAKE DEVELOPER_DIR IPHONEOS_DEPLOYMENT_TARGET
export CFLAGS CXXFLAGS

# Cargo does not treat IPHONEOS_DEPLOYMENT_TARGET as an input, so lowering or
# raising it leaves every already-built object — including the C++ ones from
# dynarmic — at the old minimum, and the core silently ships with the wrong
# LC_BUILD_VERSION. Keep a stamp and start that target triple over when it
# changes.
STAMP="$CARGO_TARGET_DIR/$TARGET/.ios-deployment-target"
if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" != "$IPHONEOS_DEPLOYMENT_TARGET" ]; then
    echo "Deployment target changed ($(cat "$STAMP") -> $IPHONEOS_DEPLOYMENT_TARGET);" \
        "rebuilding $TARGET from scratch."
    rm -rf "${CARGO_TARGET_DIR:?}/$TARGET"
fi

set -- build \
    --manifest-path "$CORE_ENTRY" \
    --target "$TARGET"

if [ "$CONFIGURATION" = Release ]; then
    set -- "$@" --release
fi

cargo "$@"

if [ "$CONFIGURATION" = Release ]; then
    PROFILE_DIR=release
else
    PROFILE_DIR=debug
fi
CORE_OUT_DIR="$CARGO_TARGET_DIR/$TARGET/$PROFILE_DIR"

mkdir -p "$(dirname -- "$STAMP")"
printf '%s' "$IPHONEOS_DEPLOYMENT_TARGET" >"$STAMP"

# Belt and braces: the stamp above prevents the stale case, this catches it if
# it ever happens anyway. A core built for a newer iOS than the app claims will
# not load at all on the older devices the app says it supports. Only the core
# this invocation built is checked — other cores' dylibs are copied into this
# directory too, and embed-cores.sh is what vets those.
built_for=$(vtool -show-build "$CORE_OUT_DIR/$CORE_DYLIB" 2>/dev/null |
    awk '/minos/ { print $2; exit }')
if [ -n "$built_for" ] && [ "$built_for" != "$IPHONEOS_DEPLOYMENT_TARGET" ]; then
    echo "error: $CORE_DYLIB was built for iOS $built_for," \
        "not $IPHONEOS_DEPLOYMENT_TARGET." >&2
    echo "       Delete $CARGO_TARGET_DIR/$TARGET and build again." >&2
    exit 1
fi

# ld leaves the string pool 4-byte aligned whenever the dylib happens to have an
# odd number of indirect symbols, and dyld then refuses to load it. See
# fix-linkedit-alignment.py.
python3 "$ROOT/scripts/fix-linkedit-alignment.py" "$CORE_OUT_DIR"/*.dylib

# A core built from another checkout is copied in beside this repository's, so
# the Embed-Cores build phase finds every core in one place.
if [ "$CORE_REPO" != "$REPO" ]; then
    HOST_OUT_DIR="$REPO/build/rust-ios-native/$TARGET/$PROFILE_DIR"
    mkdir -p "$HOST_OUT_DIR"
    for dylib in "$CORE_OUT_DIR"/*.dylib; do
        [ -f "$dylib" ] || continue
        cp -f "$dylib" "$HOST_OUT_DIR/"
        echo "Copied $(basename "$dylib") into $HOST_OUT_DIR"
    done
fi
