#!/usr/bin/env bash
# End-to-end test of the LeandrOS installer backend (userland/leandros-install)
# in QEMU:
#
#   scripts/installer-test.sh [aarch64|x86_64] [virtio|nvme]     (default: aarch64 virtio)
#
#  1. boot a GPT USB stick image with a blank install-target disk attached;
#  2. over ssh (scripts/remote.py, LEANDROS_REMOTE=qemu): `leandros-install list`
#     marks the stick "boot disk" and the target eligible; installing onto the
#     stick is refused; a dry run writes nothing; the real install emits `done`;
#  3. on the host: the GPT is valid, the copied root passes scripts/fsck-f2fs.py,
#     the home volume is a home volume, limine.conf on the new ESP carries
#     root=PARTUUID=<new p2> and no usbms.*;
#  4. boot the target ALONE (--boot-disk): root and home come from the target
#     and the login manager runs (checked over ssh and the serial log);
#  5. boot the target with the stick also attached: root still comes from the
#     target's PARTUUID.
#
# Exit 0 only when every check holds. Needs a release build (build-all.sh) and
# ~/.ssh/leandros_remote{,.pub} (remote.py keygen).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
ARCH="${1:-aarch64}"
BUS="${2:-virtio}"
case "$ARCH" in aarch64|x86_64) ;; *) echo "usage: $0 [aarch64|x86_64] [virtio|nvme]"; exit 2 ;; esac
case "$BUS" in virtio|nvme) ;; *) echo "usage: $0 [aarch64|x86_64] [virtio|nvme]"; exit 2 ;; esac
TARGET_DEV=/dev/vda; [ "$BUS" = nvme ] && TARGET_DEV=/dev/nvme0n1
WORK="$ROOT/target/installer-test-$ARCH-$BUS"
RUN_ID="installer-$ARCH-$BUS"
PORT=$((2290 + $(printf '%s' "$ARCH$BUS" | cksum | cut -d' ' -f1) % 60))
STATE="/tmp/leandros-$RUN_ID-ssh"
export LEANDROS_REMOTE="qemu:$RUN_ID"
REMOTE="python3 $ROOT/scripts/remote.py"
mkdir -p "$WORK"
QPID=""

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }
cleanup() { [ -n "$QPID" ] && kill "$QPID" 2>/dev/null || true; rm -f "$STATE"; }
trap cleanup EXIT

# boot_vm <logname> <run-qemu args...>: start QEMU headless in the background.
boot_vm() {
    local log="$WORK/$1.log"; shift
    echo "127.0.0.1 $PORT" > "$STATE"
    LEANDROS_NET=user LEANDROS_SSH_PORT=$PORT "$ROOT/scripts/run-qemu.sh" "$ARCH" --uefi --no-gpu "$@" -display none \
        > "$log" 2>&1 < /dev/null &
    QPID=$!
    local i
    for i in $(seq 1 120); do
        kill -0 "$QPID" 2>/dev/null || { tail -5 "$log"; fail "QEMU exited early ($log)"; }
        if $REMOTE run true >/dev/null 2>&1; then ok "ssh up (${i}x2s) log=$log"; return 0; fi
        sleep 2
    done
    fail "no ssh within 240 s ($log)"
}

stop_vm() {
    $REMOTE run 'sync; poweroff' >/dev/null 2>&1 || true
    local i
    for i in $(seq 1 60); do kill -0 "$QPID" 2>/dev/null || { QPID=""; return 0; }; sleep 1; done
    kill "$QPID" 2>/dev/null || true; QPID=""
}

# 0. Images: a fresh GPT stick (own copy, never the shared one) and a blank target.
echo "== stick image"
STICK="$WORK/stick.img"
rm -f "$STICK" "$WORK/target.img"
"$ROOT/scripts/write-usb.sh" "$ARCH" --image "$STICK" --gpt >"$WORK/write-usb.log" 2>&1 || { tail -5 "$WORK/write-usb.log"; fail "write-usb.sh"; }
TGT="$WORK/target.img"

# 1-2. Install from the stick.
echo "== boot the stick, install"
LEANDROS_USB_IMAGE="$STICK" boot_vm stick --usb-stick=gpt --install-target="$TGT,$BUS"
$REMOTE run 'leandros-install list --json' > "$WORK/list.json"
python3 - "$WORK/list.json" "${TARGET_DEV#/dev/}" <<'PY' || fail "list --json"
import json, sys
disks = [json.loads(l) for l in open(sys.argv[1]) if l.startswith("{")]
by = {d["name"]: d for d in disks}
tgt = by.get(sys.argv[2])
assert tgt and tgt["eligible"], f"target not eligible: {tgt}"
boots = [d for d in disks if d["boot"]]
assert len(boots) == 1 and not boots[0]["eligible"] and boots[0]["reason"] == "boot disk", boots
print("ok: list marks", boots[0]["name"], "as the boot disk; target", tgt["name"], "eligible")
PY
BOOTDISK="$(python3 -c "import json,sys; print([json.loads(l)['name'] for l in open('$WORK/list.json') if l.startswith('{') and json.loads(l)['boot']][0])")"
if $REMOTE run "leandros-install install --target /dev/$BOOTDISK --yes" >"$WORK/refuse.log" 2>&1; then
    fail "install onto the boot disk was accepted"
fi
grep -q "boot disk" "$WORK/refuse.log" || { cat "$WORK/refuse.log"; fail "refusal without 'boot disk'"; }
ok "install onto /dev/$BOOTDISK refused (boot disk)"
$REMOTE run "leandros-install install --target $TARGET_DEV --dry-run --json" > "$WORK/dry.log"
grep -q '"ev":"done","dry_run":true' "$WORK/dry.log" || fail "dry run did not finish"
ok "dry run"
$REMOTE run "leandros-install install --target $TARGET_DEV --yes --json" > "$WORK/install.log" || { tail -5 "$WORK/install.log"; fail "install failed"; }
DONE="$(grep '"ev":"done"' "$WORK/install.log" | tail -1)"
[ -n "$DONE" ] || fail "no done event"
ROOT_PUUID="$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['root_partuuid'])" "$DONE")"
DISK_GUID="$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['disk_guid'])" "$DONE")"
grep -q 'EROFS\|Read-only file system' "$WORK/stick.log" && echo "note: EROFS seen in the guest log during the install"
ok "install done: root PARTUUID $ROOT_PUUID"
stop_vm

# 3. Host-side checks of the written disk.
echo "== host checks"
python3 - "$TGT" "$WORK" "$ROOT_PUUID" "$DISK_GUID" <<'PY' || fail "target image checks"
import os, struct, sys, uuid, zlib
img, work, want_puuid, want_disk = sys.argv[1:5]
f = open(img, "rb")
f.seek(512); h = f.read(512)
assert h[:8] == b"EFI PART", "no GPT"
c = struct.unpack_from("<I", h, 16)[0]
hh = bytearray(h[:92]); hh[16:20] = b"\0" * 4
assert zlib.crc32(bytes(hh)) == c, "bad header CRC"
assert str(uuid.UUID(bytes_le=h[56:72])) == want_disk, "disk GUID mismatch"
n, esz = struct.unpack_from("<II", h, 80)
f.seek(struct.unpack_from("<Q", h, 72)[0] * 512); ents = f.read(n * esz)
assert zlib.crc32(ents) == struct.unpack_from("<I", h, 88)[0], "bad entries CRC"
parts = {}
for i in range(n):
    e = ents[i * esz:(i + 1) * esz]
    if e[:16] != b"\0" * 16:
        parts[i + 1] = (str(uuid.UUID(bytes_le=e[16:32])), struct.unpack_from("<QQ", e, 32))
assert sorted(parts) == [1, 2, 3, 4], parts
assert parts[2][0] == want_puuid, "root PARTUUID mismatch"
# Backup header.
total = os.path.getsize(img) // 512
f.seek((total - 1) * 512); assert f.read(8) == b"EFI PART", "no backup GPT"
# Extract the f2fs extent of p2 (zero blocks stay holes) for fsck-f2fs.py.
first, last = parts[2][1]
f.seek(first * 512 + 1024); sb = f.read(100)
assert struct.unpack_from("<I", sb, 0)[0] == 0xF2F52010, "p2 is not f2fs"
blocks = struct.unpack_from("<Q", sb, 36)[0]
f.seek(first * 512)
with open(work + "/root.img", "wb") as o:
    o.truncate(blocks * 4096)
    for b in range(blocks):
        d = f.read(4096)
        if d.count(0) != 4096:
            o.seek(b * 4096); o.write(d)
open(work + "/p1", "w").write(str(parts[1][1][0] * 512))
open(work + "/p4", "w").write(str(parts[4][1][0] * 512))
print("ok: GPT valid, PARTUUID matches, p2 f2fs", blocks * 4096 >> 20, "MiB")
PY
python3 "$ROOT/scripts/fsck-f2fs.py" "$WORK/root.img" > "$WORK/fsck.log" 2>&1 || { tail -8 "$WORK/fsck.log"; fail "fsck-f2fs.py on the copied root"; }
ok "fsck-f2fs.py: $(tail -1 "$WORK/fsck.log" | cut -c1-110)"
python3 "$ROOT/scripts/mkfs-f2fs-home.py" is-home "$TGT" --offset "$(cat "$WORK/p4")" >/dev/null || fail "p4 is not a leandros-home volume"
ok "p4 is a home volume"
P1="$(cat "$WORK/p1")"
mtype -i "$TGT@@$P1" ::/limine.conf > "$WORK/limine.conf" || fail "cannot read limine.conf from the new ESP"
grep -q "root=PARTUUID=$ROOT_PUUID" "$WORK/limine.conf" || fail "limine.conf lacks root=PARTUUID=$ROOT_PUUID"
! grep -q 'usbms\.' "$WORK/limine.conf" || fail "limine.conf still has usbms.*"
if [ "$BUS" = nvme ]; then grep -q 'nvme.rw=1' "$WORK/limine.conf" || fail "nvme.rw=1 missing"; fi
mdir -i "$TGT@@$P1" ::/EFI/BOOT | grep -qi 'BOOT.*EFI' || fail "EFI/BOOT/BOOT*.EFI missing"
ok "ESP: limine.conf rewritten, EFI/BOOT present"

# 4. Boot the target alone.
echo "== boot the target alone"
boot_vm alone --boot-disk="$TGT,$BUS"
CMD="$($REMOTE run 'cat /proc/cmdline' | tr -d '\r')"
case "$CMD" in *"root=PARTUUID=$ROOT_PUUID"*) ok "booted with root=PARTUUID=$ROOT_PUUID" ;; *) fail "cmdline: $CMD" ;; esac
MNT="$($REMOTE run 'cat /proc/mounts' | tr -d '\r')"
echo "$MNT" | grep -Eq "^${TARGET_DEV}2 / " || { echo "$MNT" | head -5; fail "root is not ${TARGET_DEV}2"; }
echo "$MNT" | grep -Eq "^${TARGET_DEV}[p]?4 /home " || { echo "$MNT" | head -8; fail "/home is not on the target"; }
ok "root and /home mounted from the target"
# Headless --no-gpu: init declines to start COSMIC (no software rendering) and
# leaves the serial login; ssh answering (above) and that message are the
# evidence that userland came up on the target.
grep -aq "serial login only\|greetd" "$WORK/alone.log" || fail "userland did not reach the login stage (serial log)"
ok "userland up (login stage reached)"
grep -aq "\[BLK\] boot disk: GPT disk GUID $DISK_GUID" "$WORK/alone.log" || fail "serial log: boot disk GUID is not the target's ($DISK_GUID)"
grep -aq "\[KLOG\] logging to ${TARGET_DEV}3" "$WORK/alone.log" || fail "serial log: klog not on the target's p3"
ok "serial log: boot disk GUID and klog partition are the target's"
grep -aq 'panic' "$WORK/alone.log" && fail "panic in the serial log"
stop_vm

# 5. Boot the target with the stick also attached.
echo "== boot the target with the stick attached"
LEANDROS_USB_IMAGE="$STICK" boot_vm both --boot-disk="$TGT,$BUS" --usb-stick=gpt
CMD="$($REMOTE run 'cat /proc/cmdline' | tr -d '\r')"
case "$CMD" in *"root=PARTUUID=$ROOT_PUUID"*) ;; *) fail "cmdline with stick: $CMD" ;; esac
MNT="$($REMOTE run 'cat /proc/mounts' | tr -d '\r')"
echo "$MNT" | grep -Eq "^${TARGET_DEV}2 / " || { echo "$MNT" | head -5; fail "with the stick attached, root is not ${TARGET_DEV}2"; }
echo "$MNT" | grep -Eq "^${TARGET_DEV}[p]?4 /home " || { echo "$MNT" | head -8; fail "with the stick attached, /home is not on the target"; }
ok "stick attached: root and /home still come from the target"
stop_vm

echo "PASS: installer-test $ARCH $BUS"
