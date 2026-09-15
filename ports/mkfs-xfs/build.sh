#!/usr/bin/env bash
# Build the LeandrOS mkfs.xfs — static musl, ET_EXEC, no PT_INTERP.
#
# Unlike the other ports this is not a third-party tree: the crate lives here in
# ports/mkfs-xfs and has exactly one dependency (libc, for the BLKGETSIZE64
# ioctl). Nothing is cloned and nothing is patched.
#
# Toolchain: the pinned nightly plus the zig-backed musl linker wrappers in
# scripts/, exactly as build-all.sh's build_brush does.
# -C relocation-model=static is MANDATORY: x86_64-unknown-linux-musl otherwise
# produces a static-PIE (ET_DYN) that the LeandrOS ELF loader maps at vaddr 0.
#
# Usage: ./build.sh [aarch64|x86_64|all]   (default: all)
#
# Binaries land in ports/mkfs-xfs/out/<arch>/mkfs.xfs, ready for
# scripts/mkfs-f2fs-populated.py to pack as /sbin/mkfs.xfs.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$HERE/../.." && pwd)"
PINNED_TOOLCHAIN="+nightly-2026-04-16"

build_one() {
  local arch="$1"
  local target="${arch}-unknown-linux-musl"
  echo "🧰 Building $arch mkfs.xfs..."
  (
    cd "$HERE"
    RUSTFLAGS="-C linker=$ROOT_DIR/scripts/linker-$arch-musl.sh -C link-self-contained=no -C relocation-model=static" \
      cargo "$PINNED_TOOLCHAIN" build --target "$target" --release
  )
  mkdir -p "$HERE/out/$arch"
  install -m 0755 "$HERE/target/$target/release/mkfs_xfs" "$HERE/out/$arch/mkfs.xfs"

  # The two properties the LeandrOS loader cares about. llvm-readobj comes from
  # the pinned nightly's llvm-tools component.
  local readobj
  readobj="$(find "${RUSTUP_HOME:-$HOME/.rustup}/toolchains/nightly-2026-04-16-"* \
      -name llvm-readobj -type f 2>/dev/null | head -1 || true)"
  if [[ -n "$readobj" ]]; then
    local hdr
    hdr="$("$readobj" --file-headers --program-headers "$HERE/out/$arch/mkfs.xfs")"
    grep -q "Type: Executable (0x2)" <<<"$hdr" \
      || { echo "❌ $arch: not ET_EXEC"; exit 1; }
    grep -q "PT_INTERP" <<<"$hdr" \
      && { echo "❌ $arch: has a PT_INTERP"; exit 1; }
    echo "   ET_EXEC, no PT_INTERP ✓"
  else
    echo "   ⚠️  llvm-readobj not found, skipping ELF checks"
  fi
  ls -l "$HERE/out/$arch/mkfs.xfs"
}

case "${1:-all}" in
  aarch64|x86_64) build_one "$1" ;;
  all) build_one aarch64; build_one x86_64 ;;
  *) echo "usage: $0 [aarch64|x86_64|all]" >&2; exit 2 ;;
esac
