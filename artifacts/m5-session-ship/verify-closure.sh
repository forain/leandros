#!/bin/sh
# M5 closure verifier: resolves an ELF's full transitive DT_NEEDED against the
# UNION of {m4-input-ship/<arch>/usr/lib (M4 staged set)} and
# {m3-gl-stack/sysroot-<arch>/usr/lib (M3 base+GL set)}, M4 taking precedence.
# Modeled directly on m4-input-ship/verify-closure.sh (same shape, repointed
# at the union relevant to cosmic-comp / M5). Read-only against both source
# trees. Usage: verify-closure.sh <arch> <elf>
set -e
RE=/opt/homebrew/opt/llvm/bin/llvm-readelf
M3=$HOME/code/leandros-artifacts/m3-gl-stack
M4=$HOME/code/leandros-artifacts/m4-input-ship
arch="$1"; elf="$2"
M4LIB="$M4/$arch/usr/lib"
M3LIB="$M3/sysroot-$arch/usr/lib"

resolve() {
  n="$1"
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
    src="m4-input-ship"; [ -e "$M4LIB/$n" ] || src="m3-gl-stack(sysroot)"
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
