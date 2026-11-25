#!/bin/bash
# dualboot-esp-test.sh [aarch64|x86_64]  -- espctl "shared" layout on the dual-boot fixture.
#
# Builds a shared-layout disk from a cp -c copy of the fixture image
# (~/code/leandros-artifacts/dualboot/fx-<arch>.img, sidecars kept next to it):
#   p4 private boot partition (FAT, boot type GUID, bit 63): Limine + limine.conf
#      + kernel + initrd + owner marker, exactly the stick's ESP layout
#   p5 LeandrOS root (build image), p6 LeandrOS home (build image)
#   ESP: only /EFI/leandros/{BOOT*.EFI,limine.conf,.leandros-owned} and
#        /loader/entries/leandros.conf are added.
# Then boots through systemd-boot into LeandrOS (loader.conf is switched to the
# LeandrOS entry for the test and restored byte for byte afterwards), runs the
# espctl trial -> reboot -> promote cycle, tests repair-esp, and finally checks
# that Linux's ext4 and every original ESP file are untouched.
. "$(cd "$(dirname "$0")" && pwd)/lib/clonecp.sh"
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
# QEMU-only test: the stick needs no Intel iris/ANV ship-set (write-usb.sh refuses otherwise)
export LEANDROS_USB_ALLOW_GENERIC_GPU="${LEANDROS_USB_ALLOW_GENERIC_GPU:-1}"
ARCH="${1:-aarch64}"
case "$ARCH" in aarch64|x86_64) ;; *) echo "usage: $0 [aarch64|x86_64]"; exit 2 ;; esac
FXDIR="${FX_DIR:-$HOME/code/leandros-artifacts/dualboot}"
WORK="$ROOT/target/dualboot-esp-test-$ARCH"
IMG="$WORK/disk.img"
RUN_ID="dbesp-$ARCH"
PORT=$((2400 + $(printf '%s' "$RUN_ID" | cksum | cut -d' ' -f1) % 60))
STATE="/tmp/leandros-$RUN_ID-ssh"
export LEANDROS_REMOTE="qemu:$RUN_ID" MTOOLS_SKIP_CHECK=1
REMOTE="python3 $ROOT/scripts/remote.py"
EFI=BOOTAA64.EFI; BUS=virtio; DEV=/dev/vda; PS=""
# x86_64 mirrors the laptop: the target is an NVMe disk.
[ "$ARCH" = x86_64 ] && { EFI=BOOTX64.EFI; BUS=nvme; DEV=/dev/nvme0n1; PS=p; }
BOOTGUID=0E02ED9A-FEEE-4CAB-A22F-C861AD7DE307
ROOTGUID=3F2D991B-D385-4FFF-9CF1-A0DB03062DE5
HOMEGUID=F936DEE1-99D9-4BBC-8D2A-B7447DB9F650
QPID=""
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }
cleanup() { [ -n "$QPID" ] && kill "$QPID" 2>/dev/null || true; rm -f "$STATE"; }
trap cleanup EXIT

boot_vm() {
    local log="$WORK/$1.log"
    echo "127.0.0.1 $PORT" > "$STATE"
    LEANDROS_NET=user LEANDROS_SSH_PORT=$PORT "$ROOT/scripts/run-qemu.sh" "$ARCH" --uefi --no-gpu --boot-disk="$IMG,$BUS" -display none \
        > "$log" 2>&1 < /dev/null &
    QPID=$!
    local i
    for i in $(seq 1 150); do
        kill -0 "$QPID" 2>/dev/null || { tail -5 "$log"; fail "QEMU exited early ($log)"; }
        if $REMOTE run true >/dev/null 2>&1; then ok "ssh up (${i}x2s) $1"; return 0; fi
        sleep 2
    done
    fail "no ssh within 300 s ($log)"
}
stop_vm() {
    $REMOTE run 'sync; poweroff' >/dev/null 2>&1 || true
    local i
    for i in $(seq 1 60); do kill -0 "$QPID" 2>/dev/null || { QPID=""; return 0; }; sleep 1; done
    kill "$QPID" 2>/dev/null || true; QPID=""
}
r() { $REMOTE run "$1" | tr -d '\r'; }
esp_manifest() { local d; d=$(mktemp -d); mcopy -i "$IMG@@2M" -s -n '::/*' "$d/" 2>/dev/null; (cd "$d" && find . -type f | sort | xargs shasum -a 256); rm -rf "$d"; }

echo "== prepare shared-layout disk"
rm -rf "$WORK"; mkdir -p "$WORK"
for f in "$FXDIR"/fx-$ARCH.img*; do clone_cp "$f" "$WORK/$(basename "$f" | sed "s/fx-$ARCH/disk/")"; done
ROOT_IMG="$ROOT/f2fs-data0-$ARCH.img"; HOME_IMG="$ROOT/f2fs-data1-$ARCH.img"
KERNEL="$ROOT/target/final-$ARCH/kernel"; INITRD="$ROOT/initrd-$ARCH.cpio"
LIMINE="$ROOT/.limine-cache/limine-$(sed -n 's/^LIMINE_VERSION=//p' scripts/write-usb.sh | head -1 | tr -d '"')-binary/$EFI"
[ -f "$LIMINE" ] || LIMINE=$(ls "$ROOT"/.limine-cache/limine-*-binary/$EFI | tail -1)
for f in "$ROOT_IMG" "$HOME_IMG" "$KERNEL" "$INITRD" "$LIMINE"; do [ -f "$f" ] || fail "missing $f (run build-all.sh)"; done
esp_manifest > "$WORK/esp.before"
mdir -i "$IMG@@2M" -b ::/ >/dev/null
mtype -i "$IMG@@2M" ::/loader/loader.conf > "$WORK/loader.conf.orig"
RS=$(( ($(file_size "$ROOT_IMG") + 1048575) / 1048576 )); HS=$(( ($(file_size "$HOME_IMG") + 1048575) / 1048576 ))
truncate -s +$((RS + HS + 604))M "$IMG"
sgdisk -e "$IMG" >/dev/null
P3E=$(sgdisk -i 3 "$IMG" | sed -n 's/^Last sector: *\([0-9]*\).*/\1/p')
A4=$(( (P3E + 1 + 2047) / 2048 * 2048 )); E4=$((A4 + 512 * 2048 - 1))
A5=$(( (E4 + 1 + 2047) / 2048 * 2048 )); E5=$((A5 + RS * 2048 - 1))
A6=$(( (E5 + 1 + 2047) / 2048 * 2048 )); E6=$((A6 + HS * 2048 - 1))
sgdisk -n 4:$A4:$E4 -t 4:$BOOTGUID -c 4:LeandrOS-boot -A 4:set:63 \
       -n 5:$A5:$E5 -t 5:$ROOTGUID -c 5:LeandrOS-root -A 5:set:63 \
       -n 6:$A6:$E6 -t 6:$HOMEGUID -c 6:LeandrOS-home -A 6:set:63 "$IMG" >/dev/null
pinfo() { sgdisk -i "$1" "$IMG" | sed -n "s/^$2: *//p" | head -1 | cut -d' ' -f1; }
P4U=$(sgdisk -i 4 "$IMG" | sed -n 's/^Partition unique GUID: //p'); P5U=$(sgdisk -i 5 "$IMG" | sed -n 's/^Partition unique GUID: //p')
P6U=$(sgdisk -i 6 "$IMG" | sed -n 's/^Partition unique GUID: //p')
RW=""
P1U=$(sgdisk -i 1 "$IMG" | sed -n 's/^Partition unique GUID: //p')
# NVMe is read-only unless blk.rw= lists the partition; the ESP is listed so repair-esp can run from the installed system
# (booted from the stick, which has no blk.rw=, espctl opens a BLKWRWIN window instead).
[ "$BUS" = nvme ] && RW=" blk.rw=PARTUUID=$P4U,PARTUUID=$P5U,PARTUUID=$P6U,PARTUUID=$P1U"
S4=$(pinfo 4 "First sector"); S5=$(pinfo 5 "First sector"); S6=$(pinfo 6 "First sector")
python3 - "$ROOT_IMG" "$IMG" $((S5 * 512)) "$HOME_IMG" $((S6 * 512)) <<'PY'
import os, sys
a = sys.argv[1:]
for src, off in ((a[0], int(a[2])), (a[3], int(a[4]))):
    fi = os.open(src, os.O_RDONLY); fo = os.open(a[1], os.O_WRONLY)
    size = os.fstat(fi).st_size; pos = 0
    while pos < size:
        try: d = os.lseek(fi, pos, os.SEEK_DATA)
        except OSError: break
        h = os.lseek(fi, d, os.SEEK_HOLE)
        os.lseek(fi, d, os.SEEK_SET); os.lseek(fo, off + d, os.SEEK_SET)
        n = h - d
        while n:
            b = os.read(fi, min(n, 1 << 20)); os.write(fo, b); n -= len(b)
        pos = h
PY
P4="$IMG@@$((S4 * 512))"
mformat -i "$P4" -F -T $((512 * 2048)) -v LEANDROSBT ::
mmd -i "$P4" ::/EFI ::/EFI/BOOT ::/leandros
mcopy -i "$P4" "$LIMINE" ::/EFI/BOOT/$EFI
mcopy -i "$P4" "$KERNEL" ::/kernel.elf
mcopy -i "$P4" "$INITRD" ::/initrd.gz
printf 'timeout: 0\nserial: yes\nverbose: yes\n\n/LeandrOS\n\tprotocol: limine\n\tpath: boot():/kernel.elf\n\tmodule_path: boot():/initrd.gz\n\tmodule_cmdline: initrd\n\tcmdline: root=PARTUUID=%s%s\n\tKASLR: no\n' "$(echo "$P5U" | tr A-Z a-z)" "$RW" > "$WORK/p4.conf"
mcopy -i "$P4" "$WORK/p4.conf" ::/limine.conf
printf 'leandros-boot %s\n' "$P4U" > "$WORK/owner"; mcopy -i "$P4" "$WORK/owner" ::/leandros/owner
mcopy -i "$P4" "${LEANDROS_REMOTE_KEY:-$HOME/.ssh/leandros_remote}.pub" ::/leandros/authorized_keys
# The ESP: what `leandros-install repair-esp` writes, made here by the host so
# repair-esp itself is tested separately below.
ESP="$IMG@@2M"
mmd -i "$ESP" ::/EFI/leandros
mcopy -i "$ESP" "$LIMINE" ::/EFI/leandros/$EFI
printf 'timeout: 0\nserial: yes\n\n/LeandrOS\n\tprotocol: efi\n\tpath: guid(%s):/EFI/BOOT/%s\n' "$P4U" "$EFI" > "$WORK/esp.conf"
mcopy -i "$ESP" "$WORK/esp.conf" ::/EFI/leandros/limine.conf
printf 'title LeandrOS\nefi /EFI/leandros/%s\nsort-key zz-leandros\n' "$EFI" > "$WORK/entry.conf"
mcopy -i "$ESP" "$WORK/entry.conf" ::/loader/entries/leandros.conf
printf '%s\n' "$P4U" > "$WORK/esp.owner"; mcopy -i "$ESP" "$WORK/esp.owner" ::/EFI/leandros/.leandros-owned
ok "disk ready: p4 boot $P4U, p5 root $P5U"

echo "== boot LeandrOS through systemd-boot"
printf 'default leandros.conf\ntimeout 1\nconsole-mode keep\n' > "$WORK/loader.conf.test"
mcopy -o -i "$ESP" "$WORK/loader.conf.test" ::/loader/loader.conf
boot_vm boot1
CMD=$(r 'cat /proc/cmdline'); echo "$CMD" | grep -qi "root=PARTUUID=$P5U" && ok "root from the private layout ($P5U)" || fail "cmdline: $(echo "$CMD" | sed -E 's/([A-Za-z0-9_.]*(psk|password)[A-Za-z0-9_.]*=)[^ ]*/\1<redacted>/g')"
ST=$(r 'espctl status'); echo "$ST" | head -4
echo "$ST" | grep -q '^layout=shared' && ok "espctl: shared layout" || fail "espctl did not pick the shared layout"
echo "$ST" | grep -q "^esp=${DEV}${PS}4" && ok "espctl picked the boot partition, not the ESP" || fail "wrong device"
r "espctl --dev ${DEV}${PS}1 status 2>&1; true" | grep -qi 'shared ESP' && ok "espctl refuses the shared ESP" || fail "ESP not refused"
r "espctl --dev ${DEV}${PS}1 put /etc/hostname x.txt 2>&1; true" | grep -qi 'shared ESP' && ok "espctl refuses writes to the shared ESP" || fail "ESP write not refused"

echo "== trial -> reboot -> promote"
$REMOTE push "$KERNEL" /tmp/k.elf >/dev/null; $REMOTE push "$INITRD" /tmp/i.cpio >/dev/null
r 'espctl install /tmp/k.elf --initrd /tmp/i.cpio' | tail -3
ST=$(r 'espctl status'); echo "$ST" | grep -E '^(trial|armed|default)=' | tr '\n' ' '; echo
echo "$ST" | grep -q '^armed=1' && ok "trial armed" || fail "not armed"
stop_vm; boot_vm boot2
ST=$(r 'espctl status'); echo "$ST" | grep -E '^(booted_slot|trial|armed|default)=' | tr '\n' ' '; echo
echo "$ST" | grep -q '^booted_slot=trial' && ok "booted the trial slot from the private partition" || fail "not the trial slot"
r 'espctl promote' | tail -2
ST=$(r 'espctl status'); echo "$ST" | grep -E '^(trial|good|default)=' | tr '\n' ' '; echo
echo "$ST" | grep -q '^good=[^-]' && ok "promoted: good slot set" || fail "promote failed"
stop_vm; boot_vm boot3
ST=$(r 'espctl status'); echo "$ST" | grep -q '^booted_slot=good' && ok "next boot is the good slot" || fail "good slot not booted: $(echo "$ST" | grep booted)"

echo "== repair-esp"
# Make the three files stale on the host (still bootable), then recreate them from the guest.
stop_vm
printf 'timeout: 1\nserial: yes\n\n/LeandrOS\n\tprotocol: efi\n\tpath: guid(%s):/EFI/BOOT/%s\n' "$P4U" "$EFI" > "$WORK/esp.stale"
mcopy -o -i "$ESP" "$WORK/esp.stale" ::/EFI/leandros/limine.conf
printf 'title LeandrOS stale\nefi /EFI/leandros/%s\nsort-key aa\n' "$EFI" > "$WORK/entry.stale"
mcopy -o -i "$ESP" "$WORK/entry.stale" ::/loader/entries/leandros.conf
mdel -i "$ESP" ::/EFI/leandros/.leandros-owned
boot_vm boot4
r 'leandros-install repair-esp 2>&1' | tail -1
r "espctl --dev ${DEV}${PS}1 status 2>&1; true" | grep -qi 'shared ESP' && ok "marker restored (espctl refuses the ESP again)" || fail "marker not restored"
stop_vm
mcopy -o -i "$ESP" "$WORK/loader.conf.orig" ::/loader/loader.conf
mtype -i "$ESP" ::/EFI/leandros/limine.conf | cmp - "$WORK/esp.conf" && ok "repair-esp: chain conf recreated" || fail "chain conf differs"
mtype -i "$ESP" ::/loader/entries/leandros.conf | cmp - "$WORK/entry.conf" && ok "repair-esp: BLS entry recreated" || fail "BLS entry differs"
mtype -i "$ESP" ::/EFI/leandros/.leandros-owned | cmp - "$WORK/esp.owner" && ok "repair-esp: marker recreated" || fail "marker differs"

echo "== untouched Linux + ESP"
# Everything LeandrOS added on the ESP, then the original manifest must be a subset with equal hashes.
esp_manifest > "$WORK/esp.after"
# ./NvVars is OVMF's own variable store (x86_64 firmware without pflash vars saves it onto the first ESP it boots from).
grep -v ' ./EFI/leandros/\| ./loader/entries/leandros.conf\| ./NvVars$' "$WORK/esp.after" | diff -q - "$WORK/esp.before" >/dev/null \
    && ok "non-LeandrOS ESP files byte-identical (incl. loader.conf, systemd-boot)" || fail "ESP file manifest changed"
"$ROOT/scripts/check-dualboot-fixture.sh" "$IMG" | sed -n '/ext4\|e2fsck\|sha256\|ESP\|fsck/p' | head -20 || true  # informational: pristine mode flags the added p4-p6 by design
echo "DONE"
