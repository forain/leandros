#!/bin/sh
# cosvtgate.sh guest|host — the evdev VT gate against the real cosmic-comp (no screenshots).
# guest half (runs in the live COSMIC session, started with --post, backgrounded): finds
# cosmic-comp, then on a fixed schedule `vttest chvt 2`, samples, `vttest chvt 1`, samples:
#   * while switched away cosmic-comp holds NO /dev/input/event* fd (libseat disable_seat
#     closed them, so it cannot receive input) and its input-read counter stays flat even
#     though the host half types keys during the window;
#   * after the return it holds the fds again and a probe key reaches it (rchar grows).
# host half (--hostpost, starts as the guest half does): injects keys over QMP at fixed offsets.
# Verdict lines "VTG ..." land in the serial log. Example:
#   ffsession.py aarch64 t --nofirefox --gpu virgl --put .claude/skills/run-leandros/cosvtgate.sh:/tmp/cosvtgate.sh \
#     --post "(sh /tmp/cosvtgate.sh guest >/dev/console 2>&1 &)" --hostpost "sh .claude/skills/run-leandros/cosvtgate.sh host"
if [ "$1" = host ]; then
  D=.claude/skills/run-leandros
  sleep 14                                  # away window opens at t=6 (sample A0), closes at t=22
  for k in a b c d e; do python3 $D/driver.py chord $k >/dev/null 2>&1; done   # ~t=14..19
  sleep 22                                  # return at t=22, B0 at t=30, probe key at ~t=33
  python3 $D/driver.py chord shift >/dev/null 2>&1
  sleep 8; exit 0
fi
for d in /proc/[0-9]*; do [ "$(cat $d/comm 2>/dev/null)" = cosmic-comp ] && PID=${d#/proc/}; done
[ -n "$PID" ] || { echo "VTG FAIL no cosmic-comp"; exit 1; }
# Pure shell: no dependence on the guest's grep/awk.
evfds() { n=0; for f in /proc/$PID/fd/*; do case "$(readlink $f 2>/dev/null)" in */input/event*) n=$((n+1));; esac; done; echo $n; }
rchar() { while read k v; do [ "$k" = "rchar:" ] && echo $v; done < /proc/$PID/io; }
base=$(evfds); r_base=$(rchar)
echo "VTG pid=$PID base_fds=$base"
vttest chvt 2 >/dev/null 2>&1; sleep 6
away=$(evfds); a0=$(rchar)
sleep 16; a1=$(rchar)                       # keys typed by the host in here
vttest chvt 1 >/dev/null 2>&1; sleep 8
back=$(evfds); b0=$(rchar)
sleep 6; b1=$(rchar)                        # probe key typed by the host in here
echo "VTG away_fds=$away away_rchar_delta=$((a1-a0)) back_fds=$back back_rchar_delta=$((b1-b0))"
f=0
[ "$base" -gt 0 ] || { echo "VTG FAIL no evdev fds before switch"; f=1; }
[ "$away" -eq 0 ] || { echo "VTG FAIL evdev fds held while away"; f=1; }
[ "$((a1-a0))" -lt 4096 ] || { echo "VTG FAIL input read while away"; f=1; }
[ "$back" -ge "$base" ] || { echo "VTG FAIL fds not reopened"; f=1; }
[ "$((b1-b0))" -gt 0 ] || { echo "VTG FAIL probe key not read after return"; f=1; }
[ $f -eq 0 ] && echo "VTG PASS" || echo "VTG FAIL"
