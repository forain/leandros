#!/bin/bash
# run.sh ARCH TAG WAIT [extra ffsession args...]
ARCH=$1; TAG=$2; W=$3; shift 3
cd ${WT:?set WT to the directory holding the worktrees}/ffpaint
if [ "$ARCH" = x86_64 ]; then export LEANDROS_RUN_ID=ffpaintx LEANDROS_VNC_PORT=5973; else export LEANDROS_RUN_ID=ffpaintc LEANDROS_VNC_PORT=5975; fi
export FFSESSION_OUT=/tmp/ffpaint-runs3
python3 ${SKILL:-.claude/skills/run-leandros}/ffsession.py $ARCH "$TAG" --wait "$W" \
  --url 'http://192.168.105.1:8775/yt2.html' \
  --prefs 'browser.dom.window.dump.enabled=true;media.autoplay.default=0;media.autoplay.blocking_policy=0' \
  --shot-every 10 --freeze-detect 3 --freeze-watch 120 "$@" > /tmp/ffpaint-runs3/$TAG.log 2>&1
D=/tmp/ffpaint-runs3/run-$TAG
echo "$TAG: $(cat $D/steps.json | tr -d '\n ') last=$(grep -a 'YT ' $D/ff.log | tail -1)" >> /tmp/ffpaint-runs3/summary.txt
