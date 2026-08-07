#!/bin/bash
# M4 anvil smoke test: boot, verify ship set present, launch anvil on the
# kms/udev backend, dump its log + a screenshot. Bounded; safe to background.
# Usage: m4-smoke.sh <aarch64|x86_64>
set -u
ARCH="${1:-aarch64}"
D=/Users/forain/code/leandros/.claude/skills/run-leandros/driver.py
OUT=~/code/leandros-artifacts/notes/m4-screenshots
mkdir -p "$OUT"
export LEANDROS_QEMU_EXTRA='-qmp unix:/tmp/leandros-qmp.sock,server,nowait'

echo "===== M4 SMOKE $ARCH $(date +%T) ====="
python3 "$D" stop >/dev/null 2>&1
sleep 1
python3 "$D" start "$ARCH" 2>&1 | tail -3
python3 "$D" login root root 2>&1 | tail -2
echo "----- ship set presence -----"
python3 "$D" cmd "ls -l /bin/anvil /bin/wlclient /usr/lib/libinput.so.10 /usr/lib/libxkbcommon.so.0 /usr/lib/libudev.so.1 /usr/share/X11/xkb/rules/evdev /usr/share/X11/xkb/symbols/us" 12
echo "----- launch anvil (backgrounded on guest) -----"
python3 "$D" cmd "export ANVIL_DRM_DEVICE=/dev/dri/card0; export SMITHAY_USE_LEGACY=1; export XDG_RUNTIME_DIR=/run/user/0; export RUST_LOG=info; anvil --tty-udev >/tmp/anvil.log 2>&1 &" 8
echo "----- wait, then anvil.log -----"
python3 "$D" cmd "sleep 12; echo ===LOGSIZE===; wc -c /tmp/anvil.log; echo ===TAIL===; tail -n 60 /tmp/anvil.log" 30
echo "----- screenshot after anvil start -----"
python3 "$D" screenshot "$OUT/m4-${ARCH}-anvil-start.ppm" 2>&1 | tail -2
echo "----- ps / socket check -----"
python3 "$D" cmd "ls -l /run/user/0/ 2>&1; echo ---; ps 2>&1 | head" 10
python3 "$D" stop >/dev/null 2>&1
echo "===== DONE $ARCH $(date +%T) ====="
