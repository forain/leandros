#!/bin/sh
# Generate + compile the stub libpipewire-0.3.so.0 for <arch> from a symbol list.
# Each symbol becomes a C function returning 0/NULL (safe: the daemon's runtime
# path fails at pw_main_loop_new and never dereferences any returned pointer).
# Usage: gen-stub.sh <arch> <symbols-file>   (one symbol name per line)
set -e
arch="$1"; symfile="$2"
[ -n "$arch" ] && [ -f "$symfile" ] || { echo "usage: gen-stub.sh <arch> <symbols-file>"; exit 2; }
PG=$HOME/code/leandros-artifacts/pipewire-gap
out="$PG/lib/$arch"
mkdir -p "$out"
csrc="$PG/stub/stub-$arch.c"
{
  echo '/* Auto-generated pipewire/libspa stub for LeandrOS M6.'
  echo '   Every symbol returns 0/NULL so the pipewire-rs wrapper cleanly maps it to'
  echo '   Error::CreationFailed. No struct is ever dereferenced on the daemon path. */'
  echo 'typedef long v;'
  # pw_init is variadic-ish (argc*, argv*) but a 0-return no-arg def is ABI-safe'
  while IFS= read -r s; do
    [ -n "$s" ] || continue
    printf 'v %s(void){return 0;}\n' "$s"
  done < "$symfile"
} > "$csrc"
n=$(grep -c '^v ' "$csrc")
echo "generated $csrc with $n stub functions"
zig cc -target "$arch-linux-musl" -shared -fPIC -O2 -nostdlib \
  -Wl,-soname,libpipewire-0.3.so.0 -o "$out/libpipewire-0.3.so.0" "$csrc"
ln -sf libpipewire-0.3.so.0 "$out/libpipewire-0.3.so"
echo "built $out/libpipewire-0.3.so.0"
/opt/homebrew/opt/llvm/bin/llvm-readelf -h "$out/libpipewire-0.3.so.0" | grep -E 'Type:|Machine:'
echo "exported dynsym count: $(/opt/homebrew/opt/llvm/bin/llvm-readelf --dyn-syms "$out/libpipewire-0.3.so.0" | grep -cE ' FUNC | OBJECT ')"
