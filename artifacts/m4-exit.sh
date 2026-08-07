#!/bin/bash
# M4 exit test: anvil backgrounded on guest (render loop, not foreground timeout),
# confirm it reaches a stable render loop (socket + screenshot), then launch the
# wl_shm/xdg client and screenshot the composited window. Clean QEMU lifecycle,
# serial preserved. Usage: m4-exit.sh <arch>
set -u
ARCH="${1:-aarch64}"
D=/Users/forain/code/leandros/.claude/skills/run-leandros/driver.py
OUT=~/code/leandros-artifacts/notes/m4-screenshots
mkdir -p "$OUT"
export LEANDROS_QEMU_EXTRA='-qmp unix:/tmp/leandros-qmp.sock,server,nowait'
clean() { python3 "$D" stop >/dev/null 2>&1; pkill -9 -f qemu-system 2>/dev/null; \
          rm -f /tmp/leandros-serial.sock /tmp/leandros-monitor.sock /tmp/leandros-qmp.sock /tmp/leandros-qemu.pid; sleep 2; }

echo "===== M4 EXIT $ARCH $(date +%T) ====="
clean
python3 "$D" start "$ARCH" 2>&1 | tail -2
python3 "$D" login root root 2>&1 | tail -1
echo "----- launch anvil backgrounded -----"
python3 "$D" cmd "export ANVIL_DRM_DEVICE=/dev/dri/card0; export SMITHAY_USE_LEGACY=1; export XDG_RUNTIME_DIR=/run/user/0; export RUST_LOG=info; anvil --tty-udev >/tmp/anvil.log 2>&1 &" 8
echo "----- wait 14s; check socket + anvil.log tail -----"
python3 "$D" cmd "sleep 14; echo ===SOCK===; ls -la /run/user/0/ 2>&1; echo ===ANVILTAIL===; tail -n 12 /tmp/anvil.log" 30
echo "----- screenshot A (anvil desktop) -----"
python3 "$D" screenshot "$OUT/m4-${ARCH}-A-anvil.ppm" 2>&1 | tail -2
echo "----- launch wl_shm client -----"
python3 "$D" cmd "export WAYLAND_DISPLAY=wayland-1; export XDG_RUNTIME_DIR=/run/user/0; wlclient >/tmp/wl.log 2>&1 &" 8
echo "----- wait 6s; wl.log + anvil.log tail -----"
python3 "$D" cmd "sleep 6; echo ===WLLOG===; cat /tmp/wl.log 2>&1; echo ===ANVILTAIL2===; tail -n 8 /tmp/anvil.log" 20
echo "----- screenshot B (client composited) -----"
python3 "$D" screenshot "$OUT/m4-${ARCH}-B-client.ppm" 2>&1 | tail -2
cp /tmp/leandros-serial.log "$OUT/m4-exit-${ARCH}-serial.log" 2>/dev/null && echo "serial copied"
python3 "$D" stop >/dev/null 2>&1
echo "===== DONE $ARCH $(date +%T) ====="
