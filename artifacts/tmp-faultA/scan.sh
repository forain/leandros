#!/bin/bash
# Disassemble a lib, find instructions whose file-vaddr low-12-bits == 0xC3C
LIB="$1"
OUT="$2"
/usr/bin/objdump -d --triple=aarch64 "$LIB" 2>/dev/null > "$OUT"
echo "== $LIB =="
# addresses like "   123c3c:" ; low 12 bits == 0xc3c
awk '
/^[[:space:]]*[0-9a-f]+:/ {
  line=$0
  # extract addr (hex before first colon)
  split(line, a, ":")
  addr=a[1]
  gsub(/[[:space:]]/,"",addr)
  # low 12 bits
  n=strtonum("0x" addr)
  if ((n % 4096) == 3132) {   # 0xC3C = 3132
    print line
  }
}' "$OUT"
