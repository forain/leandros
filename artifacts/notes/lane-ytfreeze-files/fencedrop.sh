#!/bin/bash
# fencedrop.sh TAG VARIANT(dstock|dpatch) AFTER_S: host virglrenderer with the
# test hook makes every fence fail AFTER_S s after the first, like a lost context.
cd /Users/forain/code/leandros/.claude/worktrees/ytfreeze
export LEANDROS_RUN_ID=ytfreeze LEANDROS_VNC_PORT=5967 FFSESSION_OUT=/tmp/ytfreeze-ff
export DYLD_LIBRARY_PATH=/tmp/ytfreeze-virgl/prefix-$2/lib LEANDROS_TEST_FENCE_FAIL_AFTER=$3
python3 .claude/skills/run-leandros/ffsession.py aarch64 "$1" --wait 150 --url 'http://192.168.105.1:8765/yt2.html' --prefs 'browser.dom.window.dump.enabled=true;media.autoplay.default=0;media.autoplay.blocking_policy=0' --post 'echo SHELL_ALIVE; uptime' > /tmp/ytfreeze-ff-$1.log 2>&1 &
FF=$!
until pid=$(pgrep -f 'qemu-system.*leandros-ytfreeze'); do sleep 2; done
sleep 5; lsof -p $pid 2>/dev/null | grep -o '/[^ ]*libvirglrenderer[^ ]*' | sort -u > /tmp/ytfreeze-ff-$1.lib
wait $FF
D=/tmp/ytfreeze-ff/run-$1
echo "$1 $2: lib=$(cat /tmp/ytfreeze-ff-$1.lib) fence_fail_log=$(grep -c 'fence sync' $D/qemu-stderr.log) hung=$(grep -a -c 'HOST GPU HUNG' $D/serial.log) cleared=$(grep -a -c 'answers fences again' $D/serial.log) shell=$(grep -c SHELL_ALIVE $D/post.txt) yt_last=$(grep -a 'YT ' $D/ff.log | tail -1)"
