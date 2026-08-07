#!/bin/bash
# Probe the guest /sys to learn why stat(/sys/dev/char/226:0/device/drm) hangs.
# All potentially-hanging commands are wrapped in `timeout` so the harness
# never blocks. Bounded; safe to background. Usage: m4-sysfs-probe.sh <arch>
set -u
ARCH="${1:-aarch64}"
D=/Users/forain/code/leandros/.claude/skills/run-leandros/driver.py

echo "===== M4 SYSFS PROBE $ARCH $(date +%T) ====="
python3 "$D" stop >/dev/null 2>&1; sleep 1
python3 "$D" start "$ARCH" 2>&1 | tail -1
python3 "$D" login root root 2>&1 | tail -1
echo "----- mounts -----"
python3 "$D" cmd "cat /proc/mounts 2>&1 || cat /proc/self/mounts 2>&1" 10
echo "----- / listing -----"
python3 "$D" cmd "ls -la /" 10
echo "----- /sys top -----"
python3 "$D" cmd "timeout 3 ls -la /sys 2>&1; echo RC=\$?" 10
echo "----- /sys/dev/char listing -----"
python3 "$D" cmd "timeout 3 ls -la /sys/dev/char 2>&1; echo RC=\$?" 10
echo "----- THE stat: /sys/dev/char/226:0/device/drm -----"
python3 "$D" cmd "timeout 4 stat /sys/dev/char/226:0/device/drm 2>&1; echo STATRC=\$?" 12
echo "----- plain stat /sys and a bogus /sys path -----"
python3 "$D" cmd "timeout 3 stat /sys 2>&1; echo RC=\$?; timeout 4 stat /sys/nonexistent_xyz 2>&1; echo BOGUSRC=\$?" 14
python3 "$D" stop >/dev/null 2>&1
echo "===== DONE $ARCH $(date +%T) ====="
