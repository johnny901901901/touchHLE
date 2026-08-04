#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
REPO=$(CDPATH= cd -- "$ROOT/../.." && pwd)
SDL_ROOT="$REPO/vendor/rust-sdl2/sdl2-sys/SDL"
SDK=${1:-iphonesimulator}

case "$SDK" in
    iphonesimulator)
        DESTINATION='generic/platform=iOS Simulator'
        ;;
    iphoneos)
        DESTINATION='generic/platform=iOS'
        ;;
    *)
        echo "Usage: $0 [iphonesimulator|iphoneos]" >&2
        exit 2
        ;;
esac

DEVELOPER_DIR=${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}
export DEVELOPER_DIR

xcodebuild \
    -project "$SDL_ROOT/Xcode/SDL/SDL.xcodeproj" \
    -scheme 'Static Library-iOS' \
    -configuration Debug \
    -sdk "$SDK" \
    -destination "$DESTINATION" \
    -derivedDataPath "$REPO/build/sdl-$SDK" \
    CODE_SIGNING_ALLOWED=NO \
    IPHONEOS_DEPLOYMENT_TARGET=15.0 \
    EXCLUDED_SOURCE_FILE_NAMES=SDL_shaders_metal.metal \
    build
