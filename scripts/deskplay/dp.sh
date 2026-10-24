#!/bin/sh
# deskplay guest helper: launch | kill | snap TAG | measure TAG N INTERVAL | blank on|off|status
D=/tmp/dp; mkdir -p $D
SESS="XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1"
URL=https://www.youtube.com/watch?v=aqz-KE-bpKQ
ffpids() { for p in /proc/[0-9]*; do k=$(cat $p/comm 2>/dev/null); case "$k" in *irefox*|*Content*|RDD*|Socket*|Utility*|Privileged*|WebExt*|Isolated*|forkserver) echo ${p#/proc/};; esac; done; }
snap() {
  f=$D/$1; { echo "=== up $(cut -d' ' -f1 /proc/uptime)"
  echo "=== rqhold"; cat /proc/rqhold
  echo "=== lockstat"; cat /proc/lockstat
  echo "=== i915"; cat /proc/i915
  echo "=== netdev"; cat /proc/net/dev
  echo "=== blank"; su leandro -c "env $SESS /bin/blanktest --status" 2>&1
  echo "=== procs"
  for p in /proc/[0-9]*; do s=$(cat $p/stat 2>/dev/null) || continue; r=${s##*) }; set -- $r; echo "P ${p#/proc/} $(tr ' ' '_' < $p/comm) ${12} ${13}"; done
  echo "=== sysstat"; cat /proc/sysstat; } > $f.tmp 2>&1; mv $f.tmp $f; }
case $1 in
launch) nohup su leandro -c "env $SESS LIBVA_MESSAGING_LEVEL=0 MOZ_AV_LOG_LEVEL=0 firefox $URL" > $D/ff.log 2>&1 < /dev/null &
        echo launched;;
kill) for i in 1 2 3; do for p in $(ffpids); do kill -9 $p 2>/dev/null; done; sleep 2; done; echo left: $(ffpids);;
snap) snap $2;;
measure) i=0; while [ $i -le $3 ]; do snap $2-$i; i=$((i+1)); [ $i -le $3 ] && sleep $4; done; echo M_DONE;;
blank) su leandro -c "env $SESS /bin/blanktest --$2" 2>&1;;
esac
