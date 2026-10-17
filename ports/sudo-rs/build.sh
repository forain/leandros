#!/usr/bin/env bash
# Build sudo-rs (sudo + su) for LeandrOS -- static musl, one arch per
# invocation: ./build.sh aarch64 | ./build.sh x86_64
#
# Upstream: https://github.com/trifectatechfoundation/sudo-rs, Apache-2.0 OR MIT,
# pinned to the commit in PIN below. The LeandrOS deltas are in patches/ (see
# README.md). PAM is pam-leandros (ports/greetd/pam-leandros), linked in as an
# rlib exactly as greetd links it; sudo-rs's `#[link(name = "pam")]` is satisfied
# by an empty libpam.a so the linker finds the name and pulls nothing.
#
# Toolchain as ports/mkfs-fat and userland/claude-run: the pinned nightly, the
# zig-cc linker wrapper, link-self-contained=no, relocation-model=static (ET_EXEC).
#
# Output: ports/sudo-rs/out/<arch>/{sudo,su}; scripts/mkfs-f2fs-populated.py
# installs both set-user-ID root (04755).
set -euo pipefail
# macOS: pick an SDK whose libSystem the linker can read (see darwin-sdk.sh).
source "$(dirname "${BASH_SOURCE[0]}")/../../scripts/darwin-sdk.sh"

if [[ $# -ne 1 || ( "$1" != "aarch64" && "$1" != "x86_64" ) ]]; then
    echo "usage: $0 <aarch64|x86_64>" >&2
    exit 1
fi
arch="$1"

PIN=e4cfa9cf7b71e287dd5046633c0f5276072d147d   # v0.2.15
REPO=https://github.com/trifectatechfoundation/sudo-rs.git

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$HERE/../.." && pwd)"
WORK="$HERE/.work"
SRC="$WORK/sudo-rs-$PIN"
toolchain="+$(sed -n 's/^channel = "\(.*\)"/\1/p' "$ROOT_DIR/rust-toolchain.toml" | head -1)"

mkdir -p "$WORK"
# Re-clone and re-apply whenever a patch is newer than the tree's stamp.
stamp="$SRC/.leandros-patched"
if [[ -d "$SRC" && -f "$stamp" ]] && [[ -n "$(find "$HERE/patches" -type f -newer "$stamp")" ]]; then
    rm -rf "$SRC"
fi
if [[ ! -f "$stamp" ]]; then
    rm -rf "$SRC"
    git init -q "$SRC"
    git -C "$SRC" fetch -q --depth 1 "$REPO" "$PIN"
    git -C "$SRC" checkout -q --detach FETCH_HEAD
    for p in "$HERE"/patches/*.patch; do
        [[ -e "$p" ]] || continue
        git -C "$SRC" apply "$p"
    done
    touch "$stamp"
fi

# The PAM provider lives in-repo (shared with greetd); refresh it every run.
# Copied outside the sudo-rs tree because it declares its own [workspace].
rm -rf "$WORK/pam-leandros"
cp -R "$ROOT_DIR/ports/greetd/pam-leandros" "$WORK/pam-leandros"

mkdir -p "$WORK/linkstub"
printf '!<arch>\n' > "$WORK/linkstub/libpam.a"

target_triple="${arch}-unknown-linux-musl"
echo "Building sudo-rs for $arch ($target_triple)..."
(
    cd "$SRC"
    RUSTFLAGS="-C linker=$ROOT_DIR/scripts/linker-$arch-musl.sh -C link-self-contained=no -C relocation-model=static -L native=$WORK/linkstub" \
        cargo "$toolchain" build --target "$target_triple" --release --bin sudo --bin su
)
out="$HERE/out/$arch"
mkdir -p "$out"
install -m 0755 "$SRC/target/$target_triple/release/sudo" "$out/sudo"
install -m 0755 "$SRC/target/$target_triple/release/su" "$out/su"
echo "built: $out/sudo $out/su"
