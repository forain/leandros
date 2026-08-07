#!/bin/sh
# Cross-build cosmic-greeter (root binary) for one arch, with the extra env the
# greeter needs beyond build-rust.sh:
#   - BINDGEN_EXTRA_CLANG_ARGS: point pam-sys's bindgen at the sysroot PAM headers
#     (bindgen already auto-adds --target from TARGET, so __linux__ is defined).
#   - VERGEN_GIT_*: the vendored tree has no .git; satisfy build.rs's vergen so it
#     does not fail trying to run git.
# Usage: build-greeter.sh <aarch64|x86_64>
set -e
arch="$1"
[ -n "$arch" ] || { echo "usage: build-greeter.sh <aarch64|x86_64>"; exit 2; }
D=/Users/forain/code/leandros-artifacts/m6-session-bins
S=/Users/forain/code/leandros-artifacts/m3-gl-stack/sysroot-$arch

export BINDGEN_EXTRA_CLANG_ARGS="-I$S/usr/include"
export VERGEN_GIT_SHA=leandros
export VERGEN_GIT_COMMIT_DATE=2026-07-26

sh "$D/build-rust.sh" src/cosmic-greeter "$arch" --no-default-features
