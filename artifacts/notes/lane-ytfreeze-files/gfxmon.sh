#!/bin/bash
OUT=$1; RID=$2
while true; do
  pid=$(pgrep -f "qemu-system.*leandros-$RID" | head -1)
  if [ -n "$pid" ]; then
    v=$(vmmap --summary $pid 2>/dev/null)
    g=$(echo "$v" | awk '/^owned unmapped \(graphics\)/{print $4} /^IOAccelerator \(graphics\)/{print "acc="$4} /^TOTAL  /{print "tot_resident="$3}' | tr '\n' ' ')
    echo "$(date +%H:%M:%S) pid=$pid gfx_owned=$g"
  fi
  sleep 60
done >> "$OUT"
