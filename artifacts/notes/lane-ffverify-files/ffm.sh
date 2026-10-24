#!/bin/sh
# ffverify measurement: per-process CPU + syscall census of Firefox, screen on / blanked / on again.
W="env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1"
tg=""
snap() {
  echo "=== SNAP $1 up=$(cut -d' ' -f1 /proc/uptime)"
  tg=""
  for p in /proc/[0-9]*; do
    p=${p#/proc/}
    c=$(tr '\0' ' ' < /proc/$p/cmdline 2>/dev/null)
    case "$c" in *firefox*) ;; *) continue;; esac
    k=$(tr ' ' '_' < /proc/$p/comm)
    echo "P $p $k $(cut -d' ' -f14,15 /proc/$p/stat)"
    tg="$tg $p"
  done
  cat /proc/sysstat > /tmp/ss; echo "SSLINES $(wc -l < /tmp/ss)"
  while read a b c rest; do
    case "$a" in
      s) case " $tg " in *" $b "*) echo "s $b $c $rest";; esac;;
      t) case " $tg " in *" $c "*) echo "t $b $c $rest";; esac;;
    esac
  done < /tmp/ss
}
names() { for t in $tg; do for d in /proc/$t/task/*; do echo "N ${d##*/} $t $(cat $d/comm 2>/dev/null)"; done; done; }
snap on0; sleep 60; snap on1
$W /bin/blanktest --off 2>&1 | tail -n 3
sleep 10; snap off0; sleep 110; snap off1
cat /proc/wakestat | tail -n 4
$W /bin/blanktest --on 2>&1 | tail -n 3
sleep 10; snap back0; sleep 50; snap back1
names
echo FFM_DONE
