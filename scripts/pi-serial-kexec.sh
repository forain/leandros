#!/usr/bin/env bash
# pi-serial-kexec.sh <kernel> <initrd> [extra cmdline...]
#
# Network-less twin of `remote.py pi-deploy`: push a kernel + initrd over the
# serial console into /boot/pi/trial (scripts/pi-serial-push.py, framed + ACKed
# + sha256-checked), then `kexec -l ... --reuse-cmdline --append=<extras>` and
# `kexec -e`, and wait for the new kernel's shell to show the extras (plus a
# per-run sxk=<nonce> marker) in /proc/cmdline. Recovery from a bad kernel is a
# power cycle (the firmware boots the card's kernel.img; this never writes it).
#
# Env: LEANDROS_PI_SERIAL (port; default /dev/ttyUSB0 on Linux, /dev/cu.usbserial-1410 on Darwin; or unix:<sock>
#      for QEMU), LEANDROS_PI_LOGIN (user:pass if the console shows login:),
#      PI_DIR (default /boot/pi). Stop any serial logger (cat) first: the port
#      is used exclusively and the push refuses while another process holds it.
set -euo pipefail
[[ $# -ge 2 ]] || { sed -n '2,15p' "$0"; exit 2; }
KERNEL=$1 INITRD=$2; shift 2
EXTRA="$* sxk=$(date +%s)"; EXTRA=${EXTRA# }
PI_DIR=${PI_DIR:-/boot/pi}
PUSH="python3 $(cd "$(dirname "$0")" && pwd)/pi-serial-push.py"
REV=$(git -C "$(dirname "$0")" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)

$PUSH --cmd "mkdir -p $PI_DIR/trial"
$PUSH "$KERNEL" "$PI_DIR/trial/kernel"
$PUSH "$INITRD" "$PI_DIR/trial/initrd"
$PUSH --cmd "echo '$REV-serial $(date +%Y-%m-%dT%H:%M:%S)' > $PI_DIR/trial/info && sync"
$PUSH --timeout 60 --cmd "/bin/kexec -l $PI_DIR/trial/kernel --initrd=$PI_DIR/trial/initrd --reuse-cmdline --append='$EXTRA'"
$PUSH --timeout "${KEXEC_TIMEOUT:-180}" --kexec-exec "$EXTRA"
