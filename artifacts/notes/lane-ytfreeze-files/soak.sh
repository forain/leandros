#!/bin/bash
# soak.sh TAG OVERRIDE(on|off) WAIT_S: looped 1080p video; display put to sleep 120 s in
cd /Users/forain/code/leandros/.claude/worktrees/ytfreeze
export LEANDROS_RUN_ID=ytfreeze LEANDROS_VNC_PORT=5967 FFSESSION_OUT=/tmp/ytfreeze-ff
[ "$2" = off ] && export ANGLE_FEATURE_OVERRIDES_DISABLED=
python3 .claude/skills/run-leandros/ffsession.py aarch64 "$1" --wait "$3" --url 'http://192.168.105.1:8765/yt2.html' --prefs 'browser.dom.window.dump.enabled=true;media.autoplay.default=0;media.autoplay.blocking_policy=0' > /tmp/ytfreeze-ff-$1.log 2>&1 &
FF=$!
until grep -q 'observing' /tmp/ytfreeze-ff-$1.log 2>/dev/null; do sleep 5; kill -0 $FF 2>/dev/null || exit 1; done
sleep 120
echo "$(date +%T) displaysleepnow" >> /tmp/ytfreeze-ff-$1.log
pmset displaysleepnow
wait $FF
D=/tmp/ytfreeze-ff/run-$1
echo "$1 override=$2: CONTEXT_LOST=$(grep -c CONTEXT_LOST $D/qemu-stderr.log) fence_fail=$(grep -c 'fence sync' $D/qemu-stderr.log) last=$(grep -a 'YT ' $D/ff.log | tail -1)"
