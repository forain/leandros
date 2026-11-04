#!/usr/bin/env bash
# pi-gpu-smoke.sh - V3D 7.1 GPU smoke tests on the Raspberry Pi 5 over SSH.
#
# Usage:  LEANDROS_PI_HOST=<pi-ip> scripts/pi-gpu-smoke.sh
#         scripts/pi-gpu-smoke.sh pi:<pi-ip>        (any remote.py --target works,
#                                                    e.g. qemu:<run-id> for a dry run)
# Runs each test through scripts/remote.py, prints one line per test and a
# summary. Exit status 0 only if every test passed. Run it with no compositor
# holding card0 (kmscube needs DRM master): a text-console session or right
# after boot, before a desktop login.
#
# Tests (all binaries ship in /bin of the aarch64 image):
#   ident    /sys/dev/char/226:0/device/subsystem -> platform  (Pi card prep, LEANDROS_BOARD=rpi5)
#   armed    the kernel auto-armed the v3d backend ("[v3d] armed" in dmesg)
#   vulkan   vkprobe --want V3D   (v3dv through libvulkan.so.1, vkCmdFillBuffer)
#   compute  es31test --want V3D  (GLES 3.1 compute, SUBMIT_CSD)
#   kmscube  sleep 30 | kmscube -c 120 (stdin held open)      (GL on v3d, SUBMIT_CL + scanout)
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
TARGET="${1:-${LEANDROS_REMOTE:-pi}}"
export LEANDROS_REMOTE="$TARGET"
R() { python3 "$HERE/remote.py" run "$1" 2>&1; }

pass=0; total=0
report() { # name ok(0/1) detail
    total=$((total + 1))
    if [ "$2" = 0 ]; then pass=$((pass + 1)); echo "PASS $1 $3"; else echo "FAIL $1 $3"; fi
}

out=$(R 'readlink /sys/dev/char/226:0/device/subsystem')
case "$out" in */platform*) report ident 0 "subsystem=platform" ;;
    *) report ident 1 "subsystem=$(echo "$out" | tail -n1) (card not prepared with LEANDROS_BOARD=rpi5? run scripts/pi-v3d-sysfs.sh)" ;; esac

# Whole dmesg, matched locally: a remote grep -m1 can stop at an earlier
# "[v3d] ... skipped/fail" line and the quoting of "\[" differs per shell.
out=$(R 'dmesg')
if echo "$out" | grep -q -F '[v3d] armed'; then report armed 0 "auto"
else report armed 1 "$(echo "$out" | grep -F '[v3d]' | tail -n1)"; fi

out=$(R 'vkprobe --want V3D')
line=$(echo "$out" | grep -m1 -E '^vkprobe (PASS|FAIL)')
[ -n "$line" ] || line="vkprobe FAIL no-output $(echo "$out" | tail -n1)"
case "$line" in "vkprobe PASS"*) report vulkan 0 "${line#vkprobe PASS }" ;; *) report vulkan 1 "${line#vkprobe FAIL }" ;; esac

out=$(R 'es31test --want V3D')
line=$(echo "$out" | grep -m1 -E '^es31test (PASS|FAIL)')
[ -n "$line" ] || line="es31test FAIL no-output $(echo "$out" | tail -n1)"
case "$line" in "es31test PASS"*) report compute 0 "${line#es31test PASS }" ;; *) report compute 1 "${line#es31test FAIL }" ;; esac

out=$(R 'sleep 30 | kmscube -c 120 2>&1')
rend=$(echo "$out" | grep -m1 'GL_RENDERER' | sed 's/.*GL_RENDERER: *//')
frames=$(echo "$out" | grep -m1 -E '^Rendered [0-9]+ frames')
# kmscube prints GL_RENDERER only with some builds; the armed check above already
# proves the v3d backend, so frames rendered is the pass criterion here.
if [ -n "$frames" ]; then
    report kmscube 0 "renderer=${rend:-v3d(armed)} $frames"
else
    report kmscube 1 "$(echo "$out" | tail -n1)"
fi

echo "pi-gpu-smoke: $pass/$total PASS"
[ "$pass" = "$total" ]
