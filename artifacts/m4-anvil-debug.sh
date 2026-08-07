#!/bin/bash
# M4 anvil debug: run anvil in the FOREGROUND under timeout with RUST_LOG=debug
# so we capture every step up to the hang/crash + a definitive exit code
# (137 = SIGKILL by timeout => hung; 139 = SEGV; 134 = abort; 101 = rust panic).
# Bounded; safe to background. Usage: m4-anvil-debug.sh <aarch64|x86_64> [rustlog]
set -u
ARCH="${1:-aarch64}"
RLOG="${2:-debug}"
D=/Users/forain/code/leandros/.claude/skills/run-leandros/driver.py
export LEANDROS_QEMU_EXTRA='-qmp unix:/tmp/leandros-qmp.sock,server,nowait'

echo "===== M4 ANVIL DEBUG $ARCH RUST_LOG=$RLOG $(date +%T) ====="
python3 "$D" stop >/dev/null 2>&1
sleep 1
python3 "$D" start "$ARCH" 2>&1 | tail -2
python3 "$D" login root root 2>&1 | tail -1
echo "----- device nodes -----"
python3 "$D" cmd "ls -l /dev/dri/card0 /dev/input/event0 /dev/input/event1" 10
echo "----- anvil foreground (timeout 22s) -----"
python3 "$D" cmd "export ANVIL_DRM_DEVICE=/dev/dri/card0; export SMITHAY_USE_LEGACY=1; export XDG_RUNTIME_DIR=/run/user/0; export RUST_LOG=$RLOG; timeout -s KILL 22 anvil --tty-udev >/tmp/anvil.log 2>&1; echo ANVIL_EXIT=\$?" 40
echo "----- anvil.log (full) -----"
python3 "$D" cmd "cat /tmp/anvil.log" 20
python3 "$D" stop >/dev/null 2>&1
echo "===== DONE $ARCH $(date +%T) ====="
