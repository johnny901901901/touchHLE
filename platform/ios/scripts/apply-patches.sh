#!/bin/sh
set -eu

# Applies the patches in platform/ios/patches to the vendored submodules.
#
# These live here as patch files rather than as edits committed to the
# submodules because `git submodule update` would silently discard working-tree
# edits, and the next build would then produce a subtly different binary with no
# indication why. Re-running this script is safe: an already-applied patch is
# detected and skipped.
#
# Called automatically by build-rust.sh.

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
REPO=$(CDPATH= cd -- "$ROOT/../.." && pwd)
PATCH_DIR="$ROOT/patches"

apply_patch() {
    patch_file=$1
    target_dir=$2
    name=$(basename -- "$patch_file")

    if [ ! -d "$target_dir" ]; then
        echo "Skipping $name: $target_dir is missing (submodule not checked out?)" >&2
        return 0
    fi

    if git -C "$target_dir" apply --reverse --check "$patch_file" 2>/dev/null; then
        echo "Already applied: $name"
        return 0
    fi

    if ! git -C "$target_dir" apply --check "$patch_file" 2>/dev/null; then
        echo "Cannot apply $name to $target_dir." >&2
        echo "The vendored code has moved on; the patch needs rebasing." >&2
        exit 1
    fi

    git -C "$target_dir" apply "$patch_file"
    echo "Applied: $name"
}

# HyperHLE's JIT: when the process carries CS_DEBUGGED, map the code region
# directly - an executable mapping plus a writable alias made with vm_remap -
# instead of trapping into a debugger that has to implement oaknut's
# `brk #0xf00d` JIT-server protocol. Without this the core only runs under
# StikDebug (iOS 17.4+) and dies on its first translated block everywhere else,
# including under TrollStore.
#
# Note it must NOT ask for one PROT_READ|PROT_WRITE|PROT_EXEC mapping: per
# mmap(2), iOS quietly returns writable-but-not-executable memory for that, and
# jumping into it kills the process with no diagnostic at all.
apply_patch "$PATCH_DIR/oaknut-self-mapped-jit.patch" "$REPO/vendor/dynarmic"
