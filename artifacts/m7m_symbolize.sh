#!/bin/sh
# M7m: symbolize the EL0 x29-chain backtrace captured in a serial log.
# Usage: m7m_symbolize.sh <serial.log>
# Extracts [BT] ret=0x... frames, subtracts the PIE base 0x200000, and runs
# llvm-addr2line against the exact packed cosmic-comp-aarch64 binary.
set -eu
LOG="${1:?serial log path}"
A2L=/opt/homebrew/opt/llvm/bin/llvm-addr2line
BIN=/Users/forain/code/leandros-artifacts/m3-gl-stack/out/cosmic-comp-aarch64
BASE=$((0x200000))
echo "== ELR / fp header =="
grep -E "\[EXC\] EL0 Fault|\[BT\] base=" "$LOG" || true
echo "== frames (ret -> symbol) =="
grep -oE "\[BT\] [0-9]+ ret=0x[0-9a-fA-F]+" "$LOG" | while read -r _tag idx ret; do
  RAW=$(printf '%s\n' "$ret" | sed 's/ret=//')
  OFF=$((RAW - BASE))
  if [ "$OFF" -gt 0 ]; then
    SYM=$("$A2L" -f -i -C -e "$BIN" $(printf '0x%x' "$OFF") 2>/dev/null | tr '\n' ' ')
  else
    SYM="(below base $ret)"
  fi
  printf '%-8s %s  ->  %s\n' "$idx" "$ret" "$SYM"
done
echo "== also symbolize ELR =="
ELR=$(grep -oE "ELR=0x[0-9a-fA-F]+" "$LOG" | head -1 | sed 's/ELR=//')
if [ -n "${ELR:-}" ]; then
  OFF=$((ELR - BASE))
  printf 'ELR %s (off 0x%x): ' "$ELR" "$OFF"
  "$A2L" -f -i -C -e "$BIN" $(printf '0x%x' "$OFF") 2>/dev/null | tr '\n' ' '; echo
fi
