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
#      LEANDROS_USB_EXTRA_CMDLINE="nvrm.start=1 <--cmdline>" (add --cmdline nvprobe.dumpvbios=1 to capture the VBIOS)
#      (duplicates resolved, last value wins; the result is printed).
#
# The in-kernel RM client self-test (lane nvclient):
#   ./scripts/nvidia-test-stick.sh /dev/diskN --quick --cmdline "nvrm.selftest=ce"
# See artifacts/notes/lane-nvclient-2026-10-08.md.
#
# NVK (lane nvuapi, the default since 2026-10-08): after the bring-up the
# kernel registers the nouveau UAPI and init runs /bin/nvk-autotest
# (nvrm.vktest=1, on by default here): Mesa's NVK opens the GPU, then nvktest
# runs info / fill / timeline / compute / triangle / triangle_depth /
# timestamps and prints the results (and a 32x32 PPM of the triangle, as
# base64) to the console, i.e. to the stick log; then nvgltest (lane nvzink)
# does the same for OpenGL ES through GBM/EGL (zink on NVK: GL_RENDERER, an
# FBO and a gbm_bo triangle, PRIME re-import, EGL native fences, dma-buf sync
# files). COSMIC on the card (zink on NVK, /bin/gpu-env) stays off on these
# autotest boots; --cmdline "nvrm.vktest=0" (or "nvrm.desktop=1") starts the
# graphical login instead. --cmdline "nvrm.vktest=0"
# turns it off; "nvrm.uapi=0" leaves the nouveau UAPI unregistered;
# "nvrm.uapi_log=2" logs every ioctl with its arguments.
# See artifacts/notes/lane-nvuapi-2026-10-08.md.
#
# Sound (lane hda): --cmdline "snd.test=1" makes init run /bin/snd-autotest
# a few seconds after the login prompt: it prints the Intel HD Audio
# controller and codec ids and the output pins the driver chose, then plays
# through /dev/pipewire (the device PipeWire uses) a 2 s 440 Hz tone, 1 s on
# the LEFT speaker only (440 Hz), 1 s on the RIGHT only (660 Hz) and 1 s at
# 44.1 kHz, twice, and prints 'sndtest: SUMMARY PASS|FAIL'. Speakers go in the
# rear GREEN jack (line out) or headphones in the front panel jack: every
# analog output plays the same stereo signal. Runs alongside nvk-autotest.
# See artifacts/notes/lane-hda-2026-10-09.md.
#   ./scripts/nvidia-test-stick.sh /dev/diskN --cmdline snd.test=1
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
fi
# The NVK test tools (ports/nvk-tests; staged as /bin/nvktest, /bin/nvgltest
# and /bin/nvkmstest by mkfs). NVK_TESTS_OUT relocates them (both scripts read it).
NVKT_STAGE="${NVK_TESTS_OUT:-${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}/nvk-tests}/stage-x86_64/usr/bin"
NVKTEST="$NVKT_STAGE/nvktest"
NVKMSTEST="$NVKT_STAGE/nvkmstest"
NVGLTEST="$NVKT_STAGE/nvgltest"
if [[ ! -x "$NVKTEST" || ports/nvk-tests/nvktest.c -nt "$NVKTEST" || ports/nvk-tests/nvktest_spv.h -nt "$NVKTEST" \
      || ! -x "$NVKMSTEST" || ports/nvk-tests/nvkmstest.c -nt "$NVKMSTEST" \
      || ! -x "$NVGLTEST" || ports/nvk-tests/nvgltest.c -nt "$NVGLTEST" ]]; then
    echo "  building nvktest + nvgltest + nvkmstest (ports/nvk-tests/build.sh x86_64; Docker; missing or older than their source)"
    ports/nvk-tests/build.sh x86_64 || echo "WARNING: nvk-tests build failed; the stick may lack /bin/nvktest, /bin/nvgltest or /bin/nvkmstest"
fi
if [[ $QUICK == 1 ]]; then
    python3 scripts/mkfs-f2fs-populated.py f2fs-data0-x86_64.img x86_64 | grep -E "NVIDIA firmware|NVK|Sizing" || true
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
CMDLINE="$(compose_cmdline "nvrm.start=1 nvrm.vktest=1" "$EXTRA")"
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
if [[ " $CMDLINE " == *" nvrm.vktest=1 "* ]]; then
    cat <<'EOF'

nvrm.vktest=1 is on: after '==== nouveau UAPI ready' the NVK test runs by
itself (init starts /bin/nvk-autotest; it waits ~20 s after the render node
appears). nvktest (Vulkan) runs first, then nvgltest (OpenGL ES through
GBM/EGL = zink on NVK). Wait for '==== nvk-autotest: done' on screen (about
2-4 minutes after the login prompt), then log in as root/root and
'sync; poweroff'. If the screen stops changing for 5 minutes, hold the power
button: the log is flushed every 2 s. Read: artifacts/notes/lane-nvuapi-2026-10-08.md
and artifacts/notes/lane-nvzink-2026-10-08.md
  grep -a 'nvuapi\|nvktest\|nvgltest\|nvk-autotest\|Xid' nvrm-boot1.log
The KMS test runs last (/bin/nvkmstest, lane nvkms): the screen shows moving
colour bars (a band at the top that changes colour every frame, a yellow bar
sliding down) four times for a few seconds each, then the text console comes
back with the '---- nvkmstest summary ----' block. Read:
artifacts/notes/lane-nvkms-2026-10-08.md
  grep -a 'nvkms\|nvkmstest' nvrm-boot1.log
EOF
fi
if [[ " $CMDLINE " == *" snd.test=1 "* ]]; then
    cat <<'EOF'

snd.test=1 is on: about 5 s after the login prompt init runs
/bin/snd-autotest. Plug speakers into the rear GREEN jack (or headphones into
the front panel jack) before booting. You should hear, twice: a 2 s beep on
both sides, a beep on the LEFT only, a higher beep on the RIGHT only, and a
1 s beep on both sides. The screen shows '[HDA] ...' lines (controller,
codec, output pins) and ends with 'sndtest: SUMMARY PASS' and
'==== snd-autotest: done'. Read: artifacts/notes/lane-hda-2026-10-09.md
  grep -a 'HDA\|sndtest\|snd-autotest\|\[PW\]' nvrm-boot1.log
EOF
fi
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
