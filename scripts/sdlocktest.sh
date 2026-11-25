#!/usr/bin/env bash
# sdlocktest.sh <image> <expect> [timeout_s]: boot the QEMU -M raspi4b build
# (build-all.sh --arch aarch64 --raspi4b, optionally with
# LEANDROS_SDHCI_KNOBS="sdhci.rderr=2") with <image> as the SD card and check
# the SD write-grant verdict on the serial console.
#   <expect> = granted | locked:<reason-substring>
# Optional 4th arg: an extended regex that must also appear in the serial log
# (e.g. 'rderr: injected read error.*' for a kernel built with sdhci.rderr=2).
# "--suite <dir>" makes f2fs images in <dir> (mkfs.f2fs, label notleandros /
# leandros-root) and runs the foreign-label (locked, label reason) and own-label
# (granted) cases; with a kernel built with LEANDROS_SDHCI_KNOBS=sdhci.rderr=2
# both also have to show the injected-error + recovery lines (RDERR=1).
# Also checks the image bytes are unchanged unless <expect> is granted
# (sha256 before/after) and, for locked, that no /dev/vd* device was exposed.
# Exit 0 = the case held. Test images are never touched by this script.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ "${1:-}" = "--suite" ]; then
    d="$2"; mkdir -p "$d"; rc=0
    for p in notleandros:notleandros leandros:leandros-root; do
        f="$d/f2fs-${p%%:*}.img"; rm -f "$f"; truncate -s 256M "$f"
        mkfs.f2fs -f -l "${p##*:}" "$f" >/dev/null 2>&1 || { echo "mkfs.f2fs failed"; exit 2; }
    done
    extra=""; [ "${RDERR:-0}" = 1 ] && extra='rderr: injected read error on block [0-9]+, retrying'
    "$0" "$d/f2fs-notleandros.img" locked:label-is-not-leandros-root 150 "$extra" || rc=1
    "$0" "$d/f2fs-leandros.img" granted 150 "$extra" || rc=1
    exit $rc
fi
img="$1"; expect="$2"; tmo="${3:-150}"; need="${4:-}"
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
if [ -n "$need" ]; then
    grep -aE "$need" "$log" | head -3 || true
    grep -aEq "$need" "$log" || { echo "missing log: $need"; ok=0; }
    if echo "$need" | grep -q rderr; then
        grep -aEq 'rderr: read of block [0-9]+ recovered' "$log" && grep -aE 'rderr: read of block' "$log" | head -1 || { echo "missing rderr recovery line"; ok=0; }
    fi
fi
echo "sha256 before=$before"; echo "sha256 after =$after"
[ "$ok" = 1 ] && echo "PASS ($expect)" || { echo "FAIL ($expect)"; exit 1; }
