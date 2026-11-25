#!/usr/bin/env bash
# Build sudo-rs (sudo + su) for LeandrOS -- dynamic musl, one arch per
# invocation: ./build.sh aarch64 | ./build.sh x86_64
#
# Upstream: https://github.com/trifectatechfoundation/sudo-rs, Apache-2.0 OR MIT,
# pinned to the commit in PIN below. The LeandrOS deltas are in patches/ (see
# README.md). PAM is Alpine's REAL Linux-PAM: sudo-rs's `#[link(name = "pam")]`
# resolves to libpam.so.0 (scripts/lib/realpam.sh); the binaries are dynamic musl
# PIEs (PT_INTERP /lib/ld-musl-<arch>.so.1) so libpam can dlopen pam_unix & co.
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
# Re-clone and re-apply whenever the patch set changed: the stamp holds a hash
# of the patch names and contents, so a removed patch counts too (a "newer
# than the stamp" test only ever saw additions and edits).
stamp="$SRC/.leandros-patched"
patch_sum="$(cd "$HERE/patches" && { ls -1; cat -- *.patch; } 2>/dev/null | { sha1sum 2>/dev/null || shasum; } | cut -c1-40)"
if [[ -d "$SRC" && -f "$stamp" ]] && [[ "$(cat "$stamp")" != "$patch_sum" ]]; then
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
    echo "$patch_sum" > "$stamp"
fi

source "$ROOT_DIR/scripts/lib/realpam.sh"
realpam_stub "$arch" "$WORK/linkstub-$arch"
DYN_TC="$ROOT_DIR/scripts/toolchain/dyn" sh "$ROOT_DIR/scripts/toolchain/dyn/gen-cargo-config.sh" "$SRC" >/dev/null
sed -i "s|^rustflags = \[|rustflags = [\n  \"-L\", \"native=$WORK/linkstub-$arch\",|" "$SRC/.cargo/config.toml"

target_triple="${arch}-unknown-linux-musl"
echo "Building sudo-rs for $arch ($target_triple)..."
(
    cd "$SRC"
    cargo "$toolchain" build --target "$target_triple" --release --bin sudo --bin su
)
out="$HERE/out/$arch"
mkdir -p "$out"
install -m 0755 "$SRC/target/$target_triple/release/sudo" "$out/sudo"
install -m 0755 "$SRC/target/$target_triple/release/su" "$out/su"
echo "built: $out/sudo $out/su"
