#!/bin/sh
# Relink musl's libc.so the way upstream does: with
# -Wl,--dynamic-list=dynamic.list.
#
# The shipped libc.so (musl 1.2.5, built host-side with `zig cc`, see
# $ART/musl-dynamic/NOTES.md) was linked without it, because zig cc rejects
# --dynamic-list and musl's ./configure silently dropped the flag. Without the
# list every libc-internal call goes through the PLT and can be interposed by
# the program: musl's setenv(.., 0) calls getenv@plt, which in Firefox
# resolves to mozglue's getenv and self-deadlocks on gEnvLock
# (artifacts/notes/rddab-2026-10-19.md). With the list, only the symbols in
# dynamic.list (malloc family, environ, optarg, timezone, ...) stay
# interposable; everything else binds locally, as on every upstream musl.
#
# Reuses the existing object files, so the result differs from the shipped
# libc.so only in symbol binding. As a guard the script first relinks WITHOUT
# the list and requires a byte-identical match with the shipped libc.so.
#
# Output: $ART/musl-dynamic/libc-dynlist/<arch>/libc.so (the shared sysroot is
# left untouched; mkfs-f2fs-populated.py prefers this file when present).
#
# Usage: scripts/musl/relink-libc.sh [aarch64] [x86_64]
set -eu

ART="${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}"
MD="$ART/musl-dynamic"
HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="$MD/src/musl-1.2.5"
OUT="$MD/libc-dynlist"

[ $# -gt 0 ] || set -- aarch64 x86_64

LLVM_BIN="${LLVM_BIN:-/opt/homebrew/opt/llvm/bin}"
OBJCOPY="${OBJCOPY:-$LLVM_BIN/llvm-objcopy}"
NM="${NM:-$LLVM_BIN/llvm-nm}"

case "$MD$HERE" in *" "*) echo "relink-libc: paths must not contain spaces" >&2; exit 1 ;; esac

for arch in "$@"; do
  build="$MD/build/$arch"
  shipped="$MD/sysroot/$arch/usr/lib/libc.so"
  [ -d "$build/obj" ] || { echo "relink-libc: no build tree $build" >&2; exit 1; }
  [ -f "$shipped" ] || { echo "relink-libc: no $shipped" >&2; exit 1; }

  tmp="$(mktemp -d)"
  # cp -p keeps the object mtimes, so make relinks without recompiling.
  cp -Rp "$build" "$tmp/b"
  rm -f "$tmp/b/lib/libc.so"

  realcc="$MD/toolchain/$arch-linux-musl-cc"
  libcc="-rtlib=none $MD/toolchain/rtlib-shim/$arch/libcompiler_rt_patched.a"
  ldauto="$(sed -n 's/^LDFLAGS_AUTO = //p' "$build/config.mak")"

  # 1. Guard: the same link without the list must reproduce the shipped file.
  PATH="$MD/toolchain:$PATH" make -s -C "$tmp/b" -f "$SRC/Makefile" srcdir="$SRC" \
    CC="$realcc" LIBCC="$libcc" lib/libc.so
  if ! cmp -s "$tmp/b/lib/libc.so" "$shipped"; then
    echo "relink-libc: $arch baseline relink differs from the shipped libc.so; refusing" >&2
    exit 1
  fi
  rm -f "$tmp/b/lib/libc.so"

  # 2. The real link, with musl's own dynamic.list.
  #
  # zig's compiler_rt carries its own stack protector: a WEAK HIDDEN
  # __stack_chk_guard and a HIDDEN __stack_chk_fail. The linker merges it with
  # musl's GLOBAL DEFAULT guard and keeps the most restrictive visibility, so
  # the guard came out LOCAL HIDDEN in libc.so even though dynamic.list names
  # it. aarch64 (and every non-TLS-canary arch) GCC code reads the canary from
  # that global, so Alpine's libvulkan.so.1 failed to relocate it. Link
  # against a compiler_rt copy whose SSP symbols are local to it: musl's
  # definitions are then the only global ones and are exported.
  rtlib="$tmp/libcompiler_rt_ssplocal.a"
  "$OBJCOPY" --localize-symbol=__stack_chk_guard --localize-symbol=__stack_chk_fail \
    "$MD/toolchain/rtlib-shim/$arch/libcompiler_rt_patched.a" "$rtlib"
  PATH="$MD/toolchain:$PATH" DYNLIST_REAL_CC="$realcc" make -s -C "$tmp/b" -f "$SRC/Makefile" srcdir="$SRC" \
    CC="$HERE/dynlist-cc.sh" LIBCC="-rtlib=none $rtlib" \
    LDFLAGS_AUTO="$ldauto -Wl,--dynamic-list=$SRC/dynamic.list" lib/libc.so
  for sym in __stack_chk_guard __stack_chk_fail; do
    if ! "$NM" -D --defined-only "$tmp/b/lib/libc.so" | grep -q " $sym\$"; then
      echo "relink-libc: $arch libc.so does not export $sym; refusing" >&2
      exit 1
    fi
  done

  mkdir -p "$OUT/$arch"
  cp "$tmp/b/lib/libc.so" "$OUT/$arch/libc.so.new"
  mv "$OUT/$arch/libc.so.new" "$OUT/$arch/libc.so"
  rm -rf "$tmp"
  echo "relink-libc: $arch -> $OUT/$arch/libc.so"
done
