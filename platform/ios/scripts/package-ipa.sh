#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
REPO=$(CDPATH= cd -- "$ROOT/../.." && pwd)
APP="$REPO/build/host-iphoneos/Build/Products/Release-iphoneos/touchHLE.app"

# Packaging modes:
#
#   (default)                   Fully unsigned. AltStore/Sideloadly/Xcode apply
#                               their own signature and entitlements.
#   --trollstore                Fakesign with get-task-allow and the memory
#                               entitlements. TrollStore keeps them when it
#                               resigns on install, and get-task-allow is what
#                               makes its "Enable JIT" option appear. Safe on
#                               every device.
#   --trollstore-permanent-jit  As above, plus dynamic-codesigning so JIT
#                               survives relaunches. A11 and older ONLY; on A12+
#                               the app crashes on launch.
MODE=unsigned
OUTPUT=

usage() {
    echo "Usage: $0 [--trollstore | --trollstore-permanent-jit] [output.ipa]"
    echo
    echo "  --trollstore                Fakesign so TrollStore can enable JIT"
    echo "                              and grant the memory entitlements."
    echo "  --trollstore-permanent-jit  Also embed dynamic-codesigning"
    echo "                              (A11 and older only)."
}

while [ $# -gt 0 ]; do
    case "$1" in
        --trollstore)
            MODE=trollstore
            ;;
        --trollstore-permanent-jit)
            MODE=permanent
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        -*)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
        *)
            if [ -n "$OUTPUT" ]; then
                echo "Unexpected extra argument: $1" >&2
                exit 2
            fi
            OUTPUT=$1
            ;;
    esac
    shift
done

case "$MODE" in
    unsigned)
        ENTITLEMENTS=
        DEFAULT_OUTPUT="$REPO/dist/touchHLE-HyperHLE-iOS-unsigned.ipa"
        ;;
    trollstore)
        ENTITLEMENTS="$ROOT/Config/TouchHLEHost-TrollStore.entitlements"
        DEFAULT_OUTPUT="$REPO/dist/touchHLE-HyperHLE-iOS-trollstore.ipa"
        ;;
    permanent)
        ENTITLEMENTS="$ROOT/Config/TouchHLEHost-TrollStore-PermanentJIT.entitlements"
        DEFAULT_OUTPUT="$REPO/dist/touchHLE-HyperHLE-iOS-trollstore-permanent-jit.ipa"
        ;;
esac

[ -n "$OUTPUT" ] || OUTPUT="$DEFAULT_OUTPUT"

case "$OUTPUT" in
    /*) ;;
    *) OUTPUT="$PWD/$OUTPUT" ;;
esac

if [ -n "$ENTITLEMENTS" ]; then
    if [ ! -f "$ENTITLEMENTS" ]; then
        echo "Missing entitlements file: $ENTITLEMENTS" >&2
        exit 1
    fi
    if ! command -v ldid >/dev/null 2>&1; then
        echo "Fakesigning needs ldid (brew install ldid)." >&2
        exit 1
    fi
fi

if [ "$MODE" = permanent ]; then
    echo "WARNING: dynamic-codesigning is gated by PPL on A12 and newer chips" >&2
    echo "         (iPhone XS/XR onwards). The app will crash on launch there." >&2
    echo "         Use --trollstore on those devices." >&2
fi

if [ ! -d "$APP" ]; then
    echo "Unsigned app not found at $APP" >&2
    echo "Run platform/ios/scripts/build-host.sh iphoneos Release first." >&2
    exit 1
fi

if codesign -dv "$APP" >/dev/null 2>&1; then
    echo "Refusing to package a signed app: $APP" >&2
    echo "Rebuild with platform/ios/scripts/build-host.sh iphoneos Release." >&2
    exit 1
fi

STAGE=$(mktemp -d "${TMPDIR:-/tmp}/touchhle-ipa.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT HUP INT TERM

mkdir -p "$STAGE/Payload" "$(dirname -- "$OUTPUT")"
ditto "$APP" "$STAGE/Payload/touchHLE.app"
STRIP_TOOL=$(xcrun --find strip)
"$STRIP_TOOL" -S -x "$STAGE/Payload/touchHLE.app/touchHLE"
if [ -n "$ENTITLEMENTS" ]; then
    # Entitlements only apply to the main executable, but every Mach-O in the
    # bundle needs a valid signature for dyld to load it. The cores live in
    # Frameworks/ and are dlopened at runtime, so fakesign those too - without
    # entitlements of their own.
    for dylib in "$STAGE/Payload/touchHLE.app/Frameworks/"*.dylib; do
        [ -e "$dylib" ] || continue
        ldid -S "$dylib"
        echo "Fakesigned $(basename -- "$dylib")"
    done
    ldid "-S$ENTITLEMENTS" "$STAGE/Payload/touchHLE.app/touchHLE"
    echo "Fakesigned touchHLE with entitlements from $ENTITLEMENTS"
fi
(
    cd "$STAGE"
    /usr/bin/zip -qry "$STAGE/touchHLE.ipa" Payload
)
mv -f "$STAGE/touchHLE.ipa" "$OUTPUT"

echo "Created unsigned IPA: $OUTPUT"
CHECKSUM="$OUTPUT.sha256"
(
    cd "$(dirname -- "$OUTPUT")"
    shasum -a 256 "$(basename -- "$OUTPUT")" >"$(basename -- "$CHECKSUM")"
)
cat "$CHECKSUM"
echo "Created checksum: $CHECKSUM"
