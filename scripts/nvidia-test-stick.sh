#!/usr/bin/env bash
# Build and write a LeandrOS USB stick for the first NVIDIA (OpenRM / GSP)
# hardware test on an x86_64 UEFI PC (RTX 3070 / GA104 class).
#
# Usage:
#   ./scripts/nvidia-test-stick.sh <device> [--yes] [--gpt] [--quick] [--cmdline "args"]
#   ./scripts/nvidia-test-stick.sh --image <out.img> [--gpt] [--quick] [--cmdline "args"]
#
#   <device>   macOS /dev/diskN (diskutil list external), Linux /dev/sdX
#   --image    write a raw image instead (dd it yourself, or test it in QEMU
#              with LEANDROS_USB_STICK=<img>)
#   --quick    reuse the existing x86_64 userland/initrd/limine pieces from a
#              previous ./scripts/build-all.sh; only rebuild libnvrm.a, the
#              kernel (feature nvidia) and the root image (to stage firmware).
#              Default: a full `./scripts/build-all.sh --arch x86_64 --nvidia`.
#   --cmdline  extra kernel arguments, composed with the defaults below.
#              May be given more than once; a key given again (here or in a
#              later --cmdline) replaces the earlier value, so
#              --cmdline "nvrm.start=0" really turns the bring-up off (the
#              kernel also takes the last value of a repeated key).
#              Examples: "nvrm.selftest=ce" (in-kernel RM client self-test:
#              copy-engine work + colour bars on screen), "nvrm.stop_after=private"
#              to bisect, "nvrm.loglevel=1" for a quieter RM, "nvrm.timeout=180".
#   --gpt / --yes  passed to scripts/write-usb.sh.
#
# What it does:
#   1. ports/openrm/fetch.sh when the pinned OpenRM source or GSP firmware is
#      missing from $OPENRM_CACHE (default ~/.cache/leandros/openrm).
#   2. Builds libnvrm.a, the x86_64 kernel with `--features nvidia`, and the
#      f2fs root image, which then carries
#      /lib/firmware/nvidia/570.86.16/gsp_{ga10x,tu10x}.bin.
#   3. scripts/write-usb.sh x86_64 ... with
#      LEANDROS_USB_EXTRA_CMDLINE="nvprobe.dumpvbios=1 nvrm.start=1 <--cmdline>"
#      (duplicates resolved, last value wins; the result is printed).
#
# The in-kernel RM client self-test (lane nvclient):
#   ./scripts/nvidia-test-stick.sh /dev/diskN --quick --cmdline "nvrm.selftest=ce"
# See artifacts/notes/lane-nvclient-2026-10-08.md.
#
# The kernel log of every boot (last 4 kept) is persisted on the stick's log
# partition; after the test read it back on this machine with
#   sudo ./scripts/read-usb-log.sh /dev/rdiskN > nvrm-boot.log     (macOS)
#   sudo ./scripts/read-usb-log.sh /dev/sdX   > nvrm-boot.log      (Linux)
# See artifacts/notes/lane-nvshim-2026-10-07.md for what each line means.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT_DIR"

die() { echo "ERROR: $*" >&2; exit 1; }

DEVICE=""
IMAGE=""
QUICK=0
EXTRA=""
PASS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --image)   IMAGE="${2:?--image needs a path}"; shift 2 ;;
        --quick)   QUICK=1; shift ;;
        --cmdline) EXTRA="${EXTRA:+$EXTRA }${2:?--cmdline needs a string}"; shift 2 ;;
        --yes|--gpt) PASS+=("$1"); shift ;;
        -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*) die "unknown option: $1" ;;
        *)  [[ -z "$DEVICE" ]] || die "more than one device given"; DEVICE="$1"; shift ;;
    esac
done
[[ -n "$DEVICE" || -n "$IMAGE" ]] || die "give a device or --image <out.img> (see --help)"
[[ -z "$DEVICE" || -z "$IMAGE" ]] || die "give either a device or --image, not both"

# shellcheck source=ports/openrm/version.sh
source "$ROOT_DIR/ports/openrm/version.sh"

echo "=== [1/4] OpenRM $OPENRM_TAG source + GSP firmware ($OPENRM_CACHE)"
if [[ ! -d "$OPENRM_SRC/src/nvidia" || ! -f "$OPENRM_FW_DIR/gsp_ga10x.bin" ]]; then
    ports/openrm/fetch.sh
fi
[[ -f "$OPENRM_FW_DIR/gsp_ga10x.bin" ]] || die "no $OPENRM_FW_DIR/gsp_ga10x.bin after fetch.sh"

echo "=== [2/4] build (libnvrm.a, kernel --features nvidia, root image with firmware)"
if [[ $QUICK == 1 ]]; then
    for f in initrd-x86_64.cpio leandros-limine-x86_64.img; do
        [[ -f "$f" ]] || die "--quick needs a previous ./scripts/build-all.sh --arch x86_64 ($f missing)"
    done
    ports/openrm/build.sh
    ports/openrm/build-kernel.sh nvidia
    python3 scripts/mkfs-f2fs-populated.py f2fs-data0-x86_64.img x86_64 | grep -E "NVIDIA firmware|Sizing" || true
else
    ./scripts/build-all.sh --arch x86_64 --nvidia
fi

echo "=== [3/4] checks"
KERNEL=target/final-x86_64/kernel
grep -aq "OpenRM 570.86.16" "$KERNEL" || die "$KERNEL was not built with --features nvidia"
echo "  kernel: $(wc -c < "$KERNEL" | tr -d ' ') bytes, OpenRM linked"
echo "  firmware: $(ls -l "$OPENRM_FW_DIR"/gsp_ga10x.bin | awk '{print $5}') bytes gsp_ga10x.bin staged at /lib/firmware/nvidia/$OPENRM_TAG/"
echo "  expected crc32 in the log: $(python3 -c "import zlib,sys;print('%#010x' % zlib.crc32(open(sys.argv[1],'rb').read()))" "$OPENRM_FW_DIR/gsp_ga10x.bin")"

# Defaults first, then every --cmdline word; a later key=value replaces an
# earlier one with the same key (in place of the first occurrence), a bare
# flag is kept once.
compose_cmdline() {
    python3 - "$@" <<'PY'
import sys
order, val = [], {}
for tok in " ".join(sys.argv[1:]).split():
    key = tok.split("=", 1)[0]
    if key not in val:
        order.append(key)
    val[key] = tok
print(" ".join(val[k] for k in order))
PY
}
CMDLINE="$(compose_cmdline "nvprobe.dumpvbios=1 nvrm.start=1" "$EXTRA")"
echo "=== [4/4] write-usb.sh (kernel cmdline: $CMDLINE)"
if [[ -n "$IMAGE" ]]; then
    LEANDROS_USB_EXTRA_CMDLINE="$CMDLINE" scripts/write-usb.sh x86_64 --image "$IMAGE" ${PASS[@]+"${PASS[@]}"}
else
    LEANDROS_USB_EXTRA_CMDLINE="$CMDLINE" scripts/write-usb.sh x86_64 "$DEVICE" ${PASS[@]+"${PASS[@]}"}
fi

cat <<EOF

Done. On the PC:
  1. Plug the stick in, power on, pick it in the firmware boot menu (UEFI).
  2. Let it boot. The [nvrm] lines scroll on the GOP console; the GPU bring-up
     runs in a kernel thread after userspace init has mounted the root (it
     waits for it), so the login prompt can appear before or between them.
  3. Wait until '==== bring-up finished: ...' is on screen, or 3 minutes,
     whichever is first (the watchdog gives up on a step after nvrm.timeout,
     default 90 s). If the screen froze, wait the 3 minutes anyway.
  4. Log in as root/root and run 'sync; poweroff' if you can; otherwise just
     hold the power button (the log is flushed before every risky step and
     every 2 s).
  5. Back here: diskutil list external; diskutil unmountDisk /dev/diskN
     sudo ./scripts/read-usb-log.sh /dev/rdiskN > nvrm-boot1.log
     grep -a 'nvrm\\|NVRM\\|NVPROBE' nvrm-boot1.log
EOF
if [[ " $CMDLINE " == *" nvrm.selftest=ce "* ]]; then
    cat <<'EOF'

nvrm.selftest=ce is on: after '==== rm_init_adapter OK' the self-test runs
for a few seconds. Watch for 8 vertical colour bars (white yellow cyan green
magenta red blue grey) in the middle of the screen, drawn by the GPU's copy
engine; they stay about 10 s (nvrm.selftest_hold), then the console text
comes back and '==== selftest PASSED: ...' follows. Then log in and
'sync; poweroff' as usual. Read: artifacts/notes/lane-nvclient-2026-10-08.md
  grep -a 'selftest\|Xid' nvrm-boot1.log
EOF
fi
