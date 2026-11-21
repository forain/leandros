#!/bin/bash
# deskplay phase 1: login, 3 Firefox launches on the pinned stream, 11-min heartbeat across them
set -u
SP=/private/tmp/claude-501/-Users-forain-code-leandros/3a5d881b-73e3-4b64-8356-b3e09ebd0877/scratchpad; D=$SP/deskplay
cd "$(dirname "$0")/../.."; export LEANDROS_REMOTE="${LEANDROS_REMOTE:-172.16.149.179}"
R="python3 scripts/remote.py"
scripts/desktop-login.sh $SP/laptop-login.env 2>&1 | tail -n 3
$R push $D/dp.sh /tmp/dp.sh 2>&1 | tail -n 1
$R run 'sh /tmp/dp.sh kill; rm -rf /tmp/dp; sh /tmp/dp.sh blank status; cut -d" " -f1 /proc/uptime' 2>&1 | tail -n 4
# Heartbeat sized for the worst case (3 x (150 s start wait + 10 s settle + 60 s measure + kill)); stopped once done.
$D/hb.sh 720 $D/hb > /dev/null 2>&1 &
HB=$!
rm -f $D/invalid
for r in 1 2 3; do
  $R run 'sh /tmp/dp.sh launch' 2>&1 | tail -n 1
  sleep 20
  # Measure only a playing window: bounded wait for the decoder (exec_vcs) to run.
  w=$($R run 'sh /tmp/dp.sh waitplay 150' 2>&1 | tail -n 1); echo "r$r: $w"
  case "$w" in PLAYING*) sleep 10;; *) echo "r$r INVALID (playback never started)"; echo r$r >> $D/invalid;; esac
  $R run "sh /tmp/dp.sh measure r$r 2 30" 2>&1 | tail -n 1
  [ $r -lt 3 ] && $R run 'sh /tmp/dp.sh kill' 2>&1 | tail -n 1
done
kill $HB 2>/dev/null; pkill -f "ping -i 1 -t 720 172.16.149.179" 2>/dev/null; wait $HB 2>/dev/null
echo "invalid runs: $(cat $D/invalid 2>/dev/null | tr '\n' ' ')"

$R run 'sh /tmp/dp.sh snap tail; sh /tmp/dp.sh blank status' 2>&1 | tail -n 2
rm -rf $D/out; mkdir -p $D/out
for f in r1-0 r1-1 r1-2 r2-0 r2-1 r2-2 r3-0 r3-1 r3-2 tail ff.log; do $R pull /tmp/dp/$f $D/out/$f > /dev/null 2>&1 || echo "pull $f failed"; done
ls $D/out $D/out/* | wc -l
