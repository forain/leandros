#!/bin/sh
# Symbolize W2DIAG return addresses against the diagnostic libgallium.
# Usage (in Alpine arm64 container): sh symbolize.sh <load_base_hex> <addr_hex...>
# load_base = runtime ELR - (pipe_get_tile_rgba file-offset + 0x20), or from the
# kernel [MMAP] base. file_off = addr - load_base; addr2line -e libgallium file_off.
L=/out/stage-diag-aarch64/usr/lib/libgallium-25.3.6.so
BASE="$1"; shift
apk add --no-cache binutils >/dev/null 2>&1
echo "pipe_get_tile_rgba file-offset:"
nm "$L" 2>/dev/null | grep -w pipe_get_tile_rgba
for a in "$@"; do
  off=$(printf '0x%x\n' $(( a - BASE )))
  printf '%s (off %s): ' "$a" "$off"
  addr2line -f -e "$L" "$off" 2>/dev/null | tr '\n' ' '
  echo
done
