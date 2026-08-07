#!/bin/sh
# M6 closure verifier: resolves an ELF's full transitive DT_NEEDED against the
# UNION of {m5-session-ship, m4-input-ship, m3-gl-stack sysroot}, precedence in
# that order (m5 shadows m4 shadows m3). Own copy per M6 lane rules (do not
# touch m4/m5's verify-closure.sh). Read-only against all three trees.
# Usage: verify-closure.sh <arch> <elf>
set -e
RE=/opt/homebrew/opt/llvm/bin/llvm-readelf
M3=$HOME/code/leandros-artifacts/m3-gl-stack
M4=$HOME/code/leandros-artifacts/m4-input-ship
M5=$HOME/code/leandros-artifacts/m5-session-ship
arch="$1"; elf="$2"
M5LIB="$M5/$arch/usr/lib"
M4LIB="$M4/$arch/usr/lib"
M3LIB="$M3/sysroot-$arch/usr/lib"

resolve() {
  n="$1"
  if [ -e "$M5LIB/$n" ]; then echo "$M5LIB/$n"; return; fi
  if [ -e "$M4LIB/$n" ]; then echo "$M4LIB/$n"; return; fi
  if [ -e "$M3LIB/$n" ]; then echo "$M3LIB/$n"; return; fi
  echo ""
}

echo "===== $(basename "$elf") ($arch) ====="
$RE -h "$elf" | grep -E "Type:|Machine:"
$RE -l "$elf" 2>/dev/null | grep -i interpreter

seen=""
queue=$($RE -d "$elf" 2>/dev/null | grep NEEDED | sed -E 's/.*\[(.*)\]/\1/')
missing=0
while [ -n "$queue" ]; do
  next=""
  for n in $queue; do
    case " $seen " in *" $n "*) continue;; esac
    seen="$seen $n"
    lib=$(resolve "$n")
    if [ -z "$lib" ]; then
      echo "  !!! MISSING: $n"
      missing=1
      continue
    fi
    src="m3-gl-stack(sysroot)"
    [ -e "$M4LIB/$n" ] && src="m4-input-ship"
    [ -e "$M5LIB/$n" ] && src="m5-session-ship"
    real=""
    if [ -L "$lib" ]; then real=$(readlink "$lib"); fi
    printf "   %-28s [%s]%s\n" "$n" "$src" "${real:+ -> $real}"
    for d in $($RE -d "$lib" 2>/dev/null | grep NEEDED | sed -E 's/.*\[(.*)\]/\1/'); do
      next="$next $d"
    done
  done
  queue="$next"
done
if [ "$missing" -eq 0 ]; then
  echo "--- closure CLOSED ($arch), no missing NEEDED entries ---"
else
  echo "--- closure INCOMPLETE ($arch) ---"
fi
