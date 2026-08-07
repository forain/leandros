#!/bin/bash
# DRM regression: kmscube must still animate after the M4 DRM ioctl additions.
# Backgrounds kmscube, takes 2 screenshots ~1.2s apart (must differ = animating).
# Usage: m4-kmscube-regr.sh <arch>
set -u
ARCH="${1:-aarch64}"
D=/Users/forain/code/leandros/.claude/skills/run-leandros/driver.py
OUT=~/code/leandros-artifacts/notes/m4-screenshots
mkdir -p "$OUT"
clean() { python3 "$D" stop >/dev/null 2>&1; pkill -9 -f qemu-system 2>/dev/null; \
          rm -f /tmp/leandros-serial.sock /tmp/leandros-monitor.sock /tmp/leandros-qemu.pid; sleep 2; }
echo "===== KMSCUBE REGR $ARCH $(date +%T) ====="
clean
python3 "$D" start "$ARCH" 2>&1 | tail -1
python3 "$D" login root root 2>&1 | tail -1
python3 "$D" cmd "setsid kmscube -D /dev/dri/card0 >/tmp/kmscube.log 2>&1 &" 8
python3 "$D" cmd "sleep 4; echo ===KCLOG===; tail -n 6 /tmp/kmscube.log" 12
python3 "$D" screenshot "$OUT/m4-kmscube-${ARCH}-1.ppm" 2>&1 | tail -1
python3 "$D" cmd "sleep 1.2" 4
python3 "$D" screenshot "$OUT/m4-kmscube-${ARCH}-2.ppm" 2>&1 | tail -1
python3 "$D" stop >/dev/null 2>&1
# byte-diff the two frames (differ => animating)
python3 - "$OUT/m4-kmscube-${ARCH}-1.ppm" "$OUT/m4-kmscube-${ARCH}-2.ppm" <<'PY'
import sys
a=open(sys.argv[1],'rb').read(); b=open(sys.argv[2],'rb').read()
d=sum(1 for x,y in zip(a,b) if x!=y)
print("FRAME_DIFF_BYTES=%d (of %d) -> %s" % (d, min(len(a),len(b)), "ANIMATING" if d>10000 else "STATIC/NO-RENDER"))
PY
echo "===== DONE $ARCH $(date +%T) ====="
