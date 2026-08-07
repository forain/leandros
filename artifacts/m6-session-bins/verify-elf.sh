#!/bin/sh
# Verify a target ELF: ET_DYN, PT_INTERP, DT_NEEDED closure against merged sysroot.
# Usage: verify-elf.sh <arch> <elf>
S3=/Users/forain/code/leandros-artifacts/m3-gl-stack/sysroot
RE=/opt/homebrew/opt/llvm/bin/llvm-readelf
arch="$1"; elf="$2"; S=$S3-$arch
echo "===== $elf ($arch) ====="
$RE -h "$elf" | grep -E "Type:|Machine:"
echo "--- INTERP ---"; $RE -l "$elf" 2>/dev/null | grep -i "interpreter"
echo "--- DT_NEEDED / SONAME / RUNPATH ---"
$RE -d "$elf" 2>/dev/null | grep -E "NEEDED|SONAME|RUNPATH|RPATH"
echo "--- NEEDED closure check (each must exist in sysroot usr/lib) ---"
for n in $($RE -d "$elf" 2>/dev/null | grep NEEDED | sed -E 's/.*\[(.*)\]/\1/'); do
  if [ -e "$S/usr/lib/$n" ] || [ -e "$S/lib/$n" ]; then st=OK; else st="!!! MISSING (host lib?)"; fi
  printf "   %-32s %s\n" "$n" "$st"
done
