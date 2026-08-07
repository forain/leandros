#!/bin/bash
# Decisive: does stat() on the EXACT deep /sys path drm-rs uses hang or ENOENT?
# Lag-robust: run target then flush echoes so driver.py's one-command lag can't
# eat the result. All stats wrapped in timeout. Usage: m4-statprobe.sh <arch>
set -u
ARCH="${1:-aarch64}"
D=/Users/forain/code/leandros/.claude/skills/run-leandros/driver.py

echo "===== M4 STATPROBE $ARCH $(date +%T) ====="
python3 "$D" stop >/dev/null 2>&1; sleep 1
python3 "$D" start "$ARCH" 2>&1 | tail -1
python3 "$D" login root root 2>&1 | tail -1
echo "----- deep stat (the drm-rs path) + intermediates, one command -----"
python3 "$D" cmd "timeout 6 stat /sys/dev/char/226:0/device/drm; echo DEEP=\$?; timeout 6 stat /sys/dev/char/226:0; echo LVL4=\$?; timeout 6 stat /sys/dev; echo LVL2=\$?; echo ALLDONE_MARKER" 40
echo "----- flush 1 -----"
python3 "$D" cmd "echo FLUSH_ONE" 10
echo "----- flush 2 -----"
python3 "$D" cmd "echo FLUSH_TWO" 10
python3 "$D" stop >/dev/null 2>&1
echo "===== DONE $ARCH $(date +%T) ====="
