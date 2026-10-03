#!/bin/bash
# hostmon.sh OUT RUNID: sample the QEMU of this run every 20 s.
OUT=$1; RID=$2
while true; do
  pid=$(pgrep -f "qemu-system.*leandros-$RID" | head -1)
  [ -z "$pid" ] && pid=$(ps -eo pid,command | grep "[q]emu-system" | grep -- "$RID" | awk '{print $1}' | head -1)
  if [ -n "$pid" ]; then
    fp=$(footprint -p $pid 2>/dev/null | grep -E 'phys_footprint:|Footprint' | head -1 | tr -s ' ')
    iosurf=$(footprint -p $pid 2>/dev/null | grep -i -E 'IOAccelerator|IOSurface|IOKit' | tr -s ' ' | head -3 | tr '\n' '|')
    rss=$(ps -o rss= -p $pid)
    agx=$(ioreg -r -c AGXAccelerator -d 1 -w0 2>/dev/null | grep -o '"In use system memory"=[0-9]*' | head -1)
    echo "$(date +%H:%M:%S) pid=$pid rss_kib=$rss $fp $agx $iosurf"
  else
    echo "$(date +%H:%M:%S) no qemu"
  fi
  sleep 20
done >> "$OUT"
