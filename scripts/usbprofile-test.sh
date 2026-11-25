#!/usr/bin/env bash
# Smoke tests for the Pi 5 USB-root staging tools; image files only, no devices.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
# fake f2fs root: only the superblock magic matters to the tool
head -c $((8<<20)) /dev/zero > "$T/root.img"
printf '\x10\x20\xf5\xf2' | dd of="$T/root.img" bs=1 seek=1024 conv=notrunc 2>/dev/null
export LEANDROS_USB_HOME_MIB=64 LEANDROS_USB_ROOT_MIB=16
W="$ROOT/scripts/write-usb-rpi5.py"
for mode in "" "--gpt"; do
    out="$(python3 "$W" --image "$T/s$mode.img" --root-image "$T/root.img" $mode)"
    pu="$(tail -n1 <<<"$out")"; [[ "$pu" == PARTUUID=* ]] || fail "no PARTUUID ($mode)"
    sfdisk -d "$T/s$mode.img" > "$T/pt" || fail "sfdisk rejects table ($mode)"
    if [[ -z "$mode" ]]; then
        grep -q "type=83" "$T/pt" && grep -q "type=da" "$T/pt" || fail "mbr types"
        grep -qi "type=ef" "$T/pt" && fail "ESP present"
        [[ "$(grep -c '^/' "$T/pt")" == 3 ]] || fail "mbr partition count"
    else
        [[ "$(grep -c '^/' "$T/pt")" == 3 ]] || fail "gpt partition count"
        grep -qi "C12A7328" "$T/pt" && fail "ESP present (gpt)"
        grep -qi "042B7470-1302-4E61-9347-A0130B276754" "$T/pt" || fail "log type"
        grep -qi "${pu#PARTUUID=}" "$T/pt" || fail "root guid mismatch"
    fi
    [[ "$(od -An -tx1 -j$((1048576+1024)) -N4 "$T/s$mode.img" | tr -d ' ')" == 1020f5f2 ]] || fail "root not at p1 ($mode)"
    strings -a "$T/s$mode.img" | grep -qi "psk\|ssid\|limine" && fail "credentials/limine in image"
done
python3 "$W" --image "$T/s.img" --root-image "$T/root.img" 2>/dev/null && fail "overwrote existing image"
[[ ! -b /dev/null ]] || true
python3 "$W" --image /dev/null --root-image "$T/root.img" --force 2>/dev/null && fail "accepted /dev/null"
python3 "$W" --device "$T/s.img" --confirm-size 1 --root-image "$T/root.img" 2>/dev/null && fail "accepted regular file as device"
python3 "$W" --device /dev/null --confirm-size 1 --root-image "$T/root.img" 2>/dev/null && fail "accepted non-block"
for d in /dev/nvme0n1 /dev/sda /dev/vda /dev/mmcblk0; do
    [[ -b $d ]] || continue
    python3 "$W" --device "$d" --confirm-size 1 --root-image "$T/root.img" 2>/dev/null && fail "accepted $d"
done
echo "write-usb-rpi5: ok"

# staging
mkdir "$T/boot"; echo cfg > "$T/boot/config.txt"; echo k > "$T/boot/batocera.img"
echo kern > "$T/k"; echo ird > "$T/i"
S="$ROOT/scripts/stage-batocera-rpi5.sh"
a=(--boot-dir "$T/boot" --partuuid 1234abcd-01 --kernel "$T/k" --initrd "$T/i")
"$S" "${a[@]}" >/dev/null
[[ "$(ls "$T/boot" | wc -l)" == 2 ]] || fail "dry run wrote files"
"$S" "${a[@]}" --apply >/dev/null
grep -qx "root=PARTUUID=1234abcd-01 root.fail=reboot espctl=0 leandros.tryboot=1 tryboot.deadline=300 sdhci.rw=0 panic.reset=5 rootwait=60" "$T/boot/leandros-cmdline.txt" || fail "cmdline"
grep -q "tryboot_a_b\|autoboot" "$T/boot/tryboot.txt" "$T/boot/leandros-cmdline.txt" && fail "forbidden key"
[[ "$(cat "$T/boot/config.txt")" == cfg ]] || fail "config.txt changed"
"$S" "${a[@]}" --apply >/dev/null 2>&1 && fail "second apply allowed"
rm "$T/boot"/leandros-* "$T/boot/tryboot.txt"; touch "$T/boot/autoboot.txt"
"$S" "${a[@]}" >/dev/null 2>&1 && fail "autoboot.txt accepted"
echo "stage-batocera-rpi5: ok"
