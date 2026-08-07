#!/bin/bash
# Run anvil with clean QEMU-lifecycle handling + robust capture. Boots, verifies
# ship set, runs anvil (timeout), dumps anvil.log via a fresh connection, and
# preserves the serial log. Usage: m4-anvil-run.sh <arch> [rustlog] [secs]
set -u
ARCH="${1:-aarch64}"
RLOG="${2:-debug}"
SECS="${3:-20}"
D=/Users/forain/code/leandros/.claude/skills/run-leandros/driver.py
OUT=~/code/leandros-artifacts/notes/m4-screenshots
mkdir -p "$OUT"
export LEANDROS_QEMU_EXTRA='-qmp unix:/tmp/leandros-qmp.sock,server,nowait'

clean() { python3 "$D" stop >/dev/null 2>&1; pkill -9 -f qemu-system 2>/dev/null; \
          rm -f /tmp/leandros-serial.sock /tmp/leandros-monitor.sock /tmp/leandros-qmp.sock /tmp/leandros-qemu.pid; sleep 2; }

echo "===== M4 ANVIL RUN $ARCH RUST_LOG=$RLOG ${SECS}s $(date +%T) ====="
clean
python3 "$D" start "$ARCH" 2>&1 | tail -2
python3 "$D" login root root 2>&1 | tail -1
echo "----- launch anvil foreground timeout ${SECS}s -----"
python3 "$D" cmd "cd /; RUST_LOG=$RLOG ANVIL_DRM_DEVICE=/dev/dri/card0 SMITHAY_USE_LEGACY=1 XDG_RUNTIME_DIR=/run/user/0 timeout -s KILL $SECS anvil --tty-udev >/tmp/anvil.log 2>&1; echo ANVIL_EXIT=\$?" $((SECS+20))
echo "----- anvil.log -----"
python3 "$D" cmd "wc -l /tmp/anvil.log; echo ===LOG===; cat /tmp/anvil.log" 25
echo "----- run/user/0 + serial copy -----"
python3 "$D" cmd "ls -la /run/user/0 2>&1" 10
cp /tmp/leandros-serial.log "$OUT/m4-anvilrun-${ARCH}-serial.log" 2>/dev/null && echo "serial copied ($(wc -l < $OUT/m4-anvilrun-${ARCH}-serial.log) lines)"
python3 "$D" stop >/dev/null 2>&1
echo "===== DONE $ARCH $(date +%T) ====="
