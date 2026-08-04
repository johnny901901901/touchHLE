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
IPHONEOS_DEPLOYMENT_TARGET=15.0
# The cc crate still adds -fembed-bitcode for Apple embedded targets, but Apple
# removed bitcode support and ld now refuses it outright ("-mllvm and
# -bitcode_bundle cannot be used together"). These env CFLAGS land after the cc
# crate's own flags, so -fembed-bitcode=off wins.
CFLAGS="${CFLAGS:-} -fembed-bitcode=off -ffile-prefix-map=$HOME=/build -fdebug-prefix-map=$HOME=/build"
CXXFLAGS="${CXXFLAGS:-} -fembed-bitcode=off -DFMT_CONSTEVAL= -ffile-prefix-map=$HOME=/build -fdebug-prefix-map=$HOME=/build"

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
# Flags go to cargo through CARGO_ENCODED_RUSTFLAGS, which is delimited by the
# unit separator rather than spaces. RUSTFLAGS and CARGO_TARGET_*_RUSTFLAGS are
# space-split, so a checkout under a path containing a space (say
# "touchHLE v3/") made rustc treat the -L directory as two input filenames and
# fail before compiling anything.
UNIT_SEPARATOR=$(printf '\037')
ENCODED_RUSTFLAGS=
add_rustflag() {
    if [ -z "$ENCODED_RUSTFLAGS" ]; then
        ENCODED_RUSTFLAGS=$1
    else
        ENCODED_RUSTFLAGS="$ENCODED_RUSTFLAGS$UNIT_SEPARATOR$1"
    fi
}

add_rustflag "-C"
add_rustflag "link-arg=-L$SDL_LIB_DIR"
add_rustflag "-C"
add_rustflag "link-arg=-Wl,-rpath,@executable_path/Frameworks"
add_rustflag "-C"
add_rustflag "link-arg=-Wl,-rpath,@loader_path"

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
add_rustflag "-C"
add_rustflag "link-arg=-Wl,-install_name,@rpath/$CORE_DYLIB"
add_rustflag "--remap-path-prefix=$HOME=/build"

for framework in AVFoundation AudioToolbox CoreBluetooth CoreGraphics \
                 CoreHaptics CoreMotion Foundation GameController Metal \
                 OpenGLES QuartzCore UIKit; do
    add_rustflag "-C"
    add_rustflag "link-arg=-framework"
    add_rustflag "-C"
    add_rustflag "link-arg=$framework"
done

# The vendored submodules need local patches (see the script for why they are
# patch files rather than commits).
sh "$ROOT/scripts/apply-patches.sh"

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
        ;;
    aarch64-apple-ios)
        SDKROOT=$(xcrun --sdk iphoneos --show-sdk-path)
        ;;
esac
# Only one target is built per invocation, so a global setting is equivalent to
# the per-target variables it replaces.
CARGO_ENCODED_RUSTFLAGS="$ENCODED_RUSTFLAGS"
export SDKROOT CARGO_ENCODED_RUSTFLAGS

export CARGO_TARGET_DIR TOUCHHLE_BOOST_ROOT
export CMAKE_TOOLCHAIN_FILE CMAKE_GENERATOR CMAKE DEVELOPER_DIR IPHONEOS_DEPLOYMENT_TARGET
export CFLAGS CXXFLAGS

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
