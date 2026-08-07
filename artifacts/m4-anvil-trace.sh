#!/bin/bash
# Trace where anvil hangs. Inline env (not export) to remove propagation as a
# variable; RUST_LOG=trace to capture the exact last operation. Bounded.
# Usage: m4-anvil-trace.sh <arch>
set -u
ARCH="${1:-aarch64}"
D=/Users/forain/code/leandros/.claude/skills/run-leandros/driver.py

echo "===== M4 ANVIL TRACE $ARCH $(date +%T) ====="
python3 "$D" stop >/dev/null 2>&1; sleep 1
python3 "$D" start "$ARCH" 2>&1 | tail -1
python3 "$D" login root root 2>&1 | tail -1
echo "----- inline-env propagation check -----"
python3 "$D" cmd "ANVIL_DRM_DEVICE=/dev/dri/card0 FOO=bar env 2>&1 | grep -E 'ANVIL|FOO'; echo ENVCHECK_DONE" 12
echo "----- anvil RUST_LOG=trace inline-env timeout 18 -----"
python3 "$D" cmd "cd /; RUST_LOG=trace ANVIL_DRM_DEVICE=/dev/dri/card0 SMITHAY_USE_LEGACY=1 XDG_RUNTIME_DIR=/run/user/0 timeout -s KILL 18 anvil --tty-udev >/tmp/anvil.log 2>&1; echo ANVIL_EXIT=\$?" 45
echo "----- anvil.log line count + LAST 55 lines -----"
python3 "$D" cmd "wc -l /tmp/anvil.log; echo ===LAST55===; tail -n 55 /tmp/anvil.log" 25
python3 "$D" stop >/dev/null 2>&1
echo "===== DONE $ARCH $(date +%T) ====="
