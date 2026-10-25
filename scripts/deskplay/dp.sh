#!/bin/sh
# deskplay guest helper: launch | kill | snap TAG | measure TAG N INTERVAL | waitplay MAXSEC | blank on|off|status
D=/tmp/dp; mkdir -p $D
SESS="XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1"
URL=https://www.youtube.com/watch?v=aqz-KE-bpKQ
# Marionette (port 2828) on by default so the Mac side can query page state
# (ytd-app, readyState) instead of taking screenshots; DP_MARIONETTE=0 disables it.
MARI=--marionette; [ "${DP_MARIONETTE:-1}" = 0 ] && MARI=
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
launch) nohup su leandro -c "env $SESS LIBVA_MESSAGING_LEVEL=0 MOZ_AV_LOG_LEVEL=0 firefox $MARI $URL" > $D/ff.log 2>&1 < /dev/null &
        echo launched;;
kill) for i in 1 2 3; do for p in $(ffpids); do kill -9 $p 2>/dev/null; done; sleep 2; done; echo left: $(ffpids);;
snap) snap $2;;
measure) i=0; while [ $i -le $3 ]; do snap $2-$i; i=$((i+1)); [ $i -le $3 ] && sleep $4; done; echo M_DONE;;
waitplay) # bounded wait until the video decoder runs (i915 exec_vcs >= 20/s over 2 s, twice in a row)
  vcs() { while read -r k v _; do [ "$k" = exec_vcs ] && echo "$v"; done < /proc/i915; }
  t=0; ok=0; a=$(vcs)
  while [ $t -lt $2 ]; do sleep 2; t=$((t+2)); b=$(vcs)
    if [ $((${b:-0}-${a:-0})) -ge 40 ]; then ok=$((ok+1)); else ok=0; fi; a=$b
    [ $ok -ge 2 ] && { echo PLAYING after ${t}s; exit 0; }
  done; echo NOPLAY after ${t}s; exit 1;;
blank) su leandro -c "env $SESS /bin/blanktest --$2" 2>&1;;
esac
