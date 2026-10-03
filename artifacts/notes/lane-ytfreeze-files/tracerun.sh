#!/bin/bash
# tracerun.sh TAG OVERRIDE(on|off): 60 s of video with MoltenVK call tracing
cd /Users/forain/code/leandros/.claude/worktrees/ytfreeze
export LEANDROS_RUN_ID=ytfreeze LEANDROS_VNC_PORT=5967 FFSESSION_OUT=/tmp/ytfreeze-ff MVK_CONFIG_TRACE_VULKAN_CALLS=1
[ "$2" = off ] && export ANGLE_FEATURE_OVERRIDES_DISABLED=
python3 .claude/skills/run-leandros/ffsession.py aarch64 "$1" --wait 60 --url 'http://192.168.105.1:8765/yt.html?src=long1080.webm' --prefs 'browser.dom.window.dump.enabled=true;media.autoplay.default=0;media.autoplay.blocking_policy=0' > /tmp/ytfreeze-ff-$1.log 2>&1
f=/tmp/ytfreeze-ff/run-$1/qemu-stderr.log
echo "$1 override=$2: WaitEvents=$(grep -c 'vkCmdWaitEvents' $f) SetEvent=$(grep -c 'vkCmdSetEvent' $f) PipelineBarrier=$(grep -c 'vkCmdPipelineBarrier' $f) QueueSubmit=$(grep -c 'vkQueueSubmit' $f) lines=$(wc -l < $f)"
