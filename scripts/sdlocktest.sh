#!/usr/bin/env bash
# sdlocktest.sh <image> <expect> [timeout_s]: boot the QEMU -M raspi4b build
# (build-all.sh --arch aarch64 --raspi4b, optionally with
# LEANDROS_SDHCI_KNOBS="sdhci.rderr=2") with <image> as the SD card and check
# the SD write-grant verdict on the serial console.
#   <expect> = granted | locked:<reason-substring>
# Also checks the image bytes are unchanged unless <expect> is granted
# (sha256 before/after) and, for locked, that no /dev/vd* device was exposed.
# Exit 0 = the case held. Test images are never touched by this script.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
img="$1"; expect="$2"; tmo="${3:-150}"
log="$(mktemp)"; trap 'rm -f "$log"' EXIT
before="$(sha256sum "$img" | cut -d' ' -f1)"
qemu-system-aarch64 -machine raspi4b -m 2G -smp 4 -accel tcg \
    -kernel "$ROOT/target/final-aarch64/kernel-direct" \
    -device "loader,file=$ROOT/initrd-aarch64.cpio,addr=0x48000000,force-raw=on" \
    -drive "if=sd,format=raw,file=$img" -net none -parallel none -no-reboot -display none \
    -serial "file:$log" -monitor none &
q=$!
for _ in $(seq "$tmo"); do
    grep -q "write grant:" "$log" 2>/dev/null && break
    kill -0 "$q" 2>/dev/null || break
    sleep 1
done
sleep 3
kill "$q" 2>/dev/null; wait "$q" 2>/dev/null
after="$(sha256sum "$img" | cut -d' ' -f1)"
verdict="$(grep -a "\[sdhci\] write grant" "$log" | tail -1)"
echo "verdict: ${verdict:-<none>}"
ok=1
case "$expect" in
    granted) grep -aq "write grant: GRANTED" "$log" || ok=0 ;;
    locked:*)
        grep -aq "write grant: LOCKED.*${expect#locked:}" "$log" || ok=0
        [ "$before" = "$after" ] || { echo "IMAGE CHANGED"; ok=0; }
        grep -aq "whole card exposed\|exposed as /dev/vdc" "$log" && { echo "device exposed while locked"; ok=0; }
        ;;
esac
echo "sha256 before=$before"; echo "sha256 after =$after"
[ "$ok" = 1 ] && echo "PASS ($expect)" || { echo "FAIL ($expect)"; exit 1; }
