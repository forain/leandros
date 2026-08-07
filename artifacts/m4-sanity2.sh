#!/bin/bash
# Isolate: is the image sound + is the synthetic /sys skeleton read_dir-safe,
# BEFORE involving anvil? Clean QEMU state first. Preserve the serial log.
# Usage: m4-sanity2.sh <arch>
set -u
ARCH="${1:-aarch64}"
D=/Users/forain/code/leandros/.claude/skills/run-leandros/driver.py
OUT=~/code/leandros-artifacts/notes/m4-screenshots
mkdir -p "$OUT"

echo "===== M4 SANITY2 $ARCH $(date +%T) ====="
python3 "$D" stop >/dev/null 2>&1
pkill -9 -f qemu-system 2>/dev/null
rm -f /tmp/leandros-serial.sock /tmp/leandros-monitor.sock /tmp/leandros-qemu.pid
sleep 2
python3 "$D" start "$ARCH" 2>&1 | tail -2
python3 "$D" login root root 2>&1 | tail -1
echo "----- /sys skeleton read (the read_dir anvil does) -----"
python3 "$D" cmd "ls -la /sys; echo ---; ls -la /sys/dev/char; echo ---; ls -la /sys/dev/char/226:0/device/drm; echo LSRC=\$?" 15
echo "----- flush -----"
python3 "$D" cmd "echo FLUSH_A" 8
echo "----- device nodes + anvil binary -----"
python3 "$D" cmd "ls -l /dev/dri/card0 /dev/input/event0 /dev/input/event1 /bin/anvil" 12
echo "----- serial log copy -----"
cp /tmp/leandros-serial.log "$OUT/m4-sanity2-${ARCH}-serial.log" 2>/dev/null && echo "serial copied ($(wc -l < $OUT/m4-sanity2-${ARCH}-serial.log) lines)"
python3 "$D" stop >/dev/null 2>&1
echo "===== DONE $ARCH $(date +%T) ====="
