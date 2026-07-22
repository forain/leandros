#!/bin/sh
# Link a DYNAMIC (PT_INTERP) musl executable or a PIC shared object, bypassing
# zig cc's driver-level auto-management of musl CRT/libc (see NOTES.md
# "zig-cc dynamic-link landmine"). Uses `zig ld.lld` directly against our
# from-source sysroot.
#
# Usage:
#   musl-dyn-link.sh <arch: x86_64|aarch64> <mode: exe|shared> <sysroot> <output> <obj...> [-- extra-ld-args...]
set -e

arch="$1"; shift
mode="$1"; shift
sysroot="$1"; shift
output="$1"; shift

objs=""
extra=""
in_extra=0
for a in "$@"; do
  if [ "$in_extra" = "1" ]; then
    extra="$extra $a"
  elif [ "$a" = "--" ]; then
    in_extra=1
  else
    objs="$objs $a"
  fi
done

case "$arch" in
  x86_64)  elfmach=elf_x86_64 ;;
  aarch64) elfmach=aarch64linux ;;
  *) echo "unknown arch $arch" >&2; exit 1 ;;
esac
interp="/lib/ld-musl-${arch}.so.1"

if [ "$mode" = "shared" ]; then
  # shellcheck disable=SC2086
  exec zig ld.lld --error-limit=0 --sysroot="$sysroot" \
    --eh-frame-hdr -znow -m "$elfmach" -shared \
    -o "$output" \
    "$sysroot/usr/lib/crti.o" $objs "$sysroot/usr/lib/crtn.o" \
    -L"$sysroot/usr/lib" -lc $extra
else
  # dynamic PIE executable
  # shellcheck disable=SC2086
  exec zig ld.lld --error-limit=0 --sysroot="$sysroot" \
    --entry _start --build-id=none --eh-frame-hdr -znow -m "$elfmach" \
    --dynamic-linker "$interp" -pie \
    -o "$output" \
    "$sysroot/usr/lib/Scrt1.o" "$sysroot/usr/lib/crti.o" $objs "$sysroot/usr/lib/crtn.o" \
    -L"$sysroot/usr/lib" -lc $extra
fi
