#!/bin/sh
# cosvtgate.sh guest|host — the evdev VT gate against the real cosmic-comp (no screenshots).
# guest half (runs in the live COSMIC session, started with --post, backgrounded): finds
# cosmic-comp, then on a fixed schedule `vttest chvt 2`, samples, `vttest chvt 1`, samples:
#   * while switched away cosmic-comp holds NO /dev/input/event* fd (libseat disable_seat
#     closed them, so it cannot receive input), also after the host half typed keys;
#   * after the return it holds the fds again.
# (No /proc/<pid>/io here, and device fd links are empty, so the fds are counted by empty link.)
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
# /proc/<pid>/fd links of device nodes read back EMPTY here (readlink of a char device), so
# device fds are counted as the fds with an empty link target (DRM + evdev + tty).
evfds_of() { n=0; for f in /proc/$1/fd/*; do [ -z "$(readlink $f 2>/dev/null)" ] && n=$((n+1)); done; echo $n; }
# The session's cosmic-comp is the instance holding evdev fds (the greeter's nested one holds none).
PID=; best=0
for d in /proc/[0-9]*; do
  [ "$(cat $d/comm 2>/dev/null)" = cosmic-comp ] || continue
  n=$(evfds_of ${d#/proc/}); [ "$n" -gt "$best" ] && { best=$n; PID=${d#/proc/}; }
done
[ -n "$PID" ] || { echo "VTG FAIL no cosmic-comp holding evdev fds"; exit 1; }
evfds() { evfds_of $PID; }
base=$(evfds)
echo "VTG pid=$PID base_fds=$base"
vttest chvt 2 >/dev/null 2>&1; sleep 6
away=$(evfds)
sleep 16                                    # keys typed by the host in here reach only VT 2
away2=$(evfds)
vttest chvt 1 >/dev/null 2>&1; sleep 8
back=$(evfds)
echo "VTG away_fds=$away away_fds_after_keys=$away2 back_fds=$back"
f=0
[ "$base" -gt 0 ] || { echo "VTG FAIL no device fds before switch"; f=1; }
[ "$away" -lt "$base" ] && [ "$away2" -lt "$base" ] || { echo "VTG FAIL device fds (evdev) not closed while away"; f=1; }
[ "$back" -ge "$base" ] || { echo "VTG FAIL fds not reopened"; f=1; }
[ $f -eq 0 ] && echo "VTG PASS" || echo "VTG FAIL"
