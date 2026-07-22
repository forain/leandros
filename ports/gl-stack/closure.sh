#!/bin/sh
# Transitive DT_NEEDED closure of an ELF against the merged sysroot, plus known
# runtime dlopen additions. Prints the runtime-install manifest.
# Usage: closure.sh <arch> <elf> [extra dlopen soname ...]
D=/Users/forain/.claude-forain/jobs/afde2e74/tmp/m3-gl-stack
RE=/opt/homebrew/opt/llvm/bin/llvm-readelf
arch="$1"; shift; elf="$1"; shift; S=$D/sysroot-$arch
seen=""
queue=$($RE -d "$elf" 2>/dev/null | grep NEEDED | sed -E 's/.*\[(.*)\]/\1/')
queue="$queue $*"
while [ -n "$queue" ]; do
  next=""
  for n in $queue; do
    case " $seen " in *" $n "*) continue;; esac
    seen="$seen $n"
    lib=""
    [ -e "$S/usr/lib/$n" ] && lib="$S/usr/lib/$n"
    [ -z "$lib" ] && [ -e "$S/lib/$n" ] && lib="$S/lib/$n"
    if [ -n "$lib" ]; then
      for d in $($RE -d "$lib" 2>/dev/null | grep NEEDED | sed -E 's/.*\[(.*)\]/\1/'); do
        next="$next $d"
      done
    fi
  done
  queue="$next"
done
echo "Runtime closure for $(basename $elf) [$arch]:"
for n in $seen; do
  real=$(cd "$S/usr/lib" 2>/dev/null && ls -l "$n" 2>/dev/null | sed -E 's/.*-> //')
  loc="usr/lib"; [ -e "$S/usr/lib/$n" ] || loc="lib"
  [ -e "$S/usr/lib/$n" ] || [ -e "$S/lib/$n" ] || loc="MISSING"
  printf "   %-28s (%s)%s\n" "$n" "$loc" "${real:+ -> $real}"
done
