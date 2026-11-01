#!/usr/bin/env bash
# dualboot-free-test.sh [aarch64|x86_64]  -- free-space dual-boot install (v1a) in QEMU.
#
#  1. clone the gap fixture (FX_GAP_GIB=24, make-dualboot-fixture.sh) as an NVMe
#     target; boot the LeandrOS stick (write-usb.sh image) with it attached;
#  2. over ssh: list --json reports the gap and dualboot=true; a dry run prints
#     the new GPT and ESP file list and changes nothing; the real install
#     (`--mode dualboot --use-free`) emits every step;
#  3. on the host: check-dualboot-fixture.sh --installed (ext4, manifest, swap,
#     ESP originals unchanged; new partitions in the gap with bit 63; chain);
#  4. boot the fixture alone: systemd-boot's default boots Linux
#     (LINUX-DUALBOOT-OK);
#  5. on a scratch copy switch loader.conf to the LeandrOS entry: LeandrOS boots
#     with root on the new partition, blk.rw= on the cmdline, and writes to the
#     Linux partitions fail with EROFS; loader.conf is restored and the
#     fixture checker passes again.
#
# Env: FX_DIR (default ~/code/leandros-artifacts/dualboot), LEANDROS_INSTALL_BIN
# (a freshly built leandros-install pushed into the stick before use).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
ARCH="${1:-aarch64}"
case "$ARCH" in aarch64|x86_64) ;; *) echo "usage: $0 [aarch64|x86_64]"; exit 2 ;; esac
FXDIR="${FX_DIR:-$HOME/code/leandros-artifacts/dualboot}"
WORK="$ROOT/target/dualboot-free-test-$ARCH"
IMG="$WORK/disk.img"
RUN_ID="dbfree-$ARCH"
PORT=$((2500 + $(printf '%s' "$RUN_ID" | cksum | cut -d' ' -f1) % 60))
STATE="/tmp/leandros-$RUN_ID-ssh"
export LEANDROS_REMOTE="qemu:$RUN_ID" MTOOLS_SKIP_CHECK=1
REMOTE="python3 $ROOT/scripts/remote.py"
BIN="${LEANDROS_INSTALL_BIN:-$ROOT/userland/leandros-install/target/$ARCH-unknown-linux-musl/release/leandros-install}"
QPID=""
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }
cleanup() { [ -n "$QPID" ] && kill "$QPID" 2>/dev/null || true; rm -f "$STATE"; }
trap cleanup EXIT

boot_vm() { # <log> <run-qemu args...>
    local log="$WORK/$1.log"; shift
    echo "127.0.0.1 $PORT" > "$STATE"
    LEANDROS_NET=user LEANDROS_SSH_PORT=$PORT "$ROOT/scripts/run-qemu.sh" "$ARCH" --uefi --no-gpu "$@" -display none \
        > "$log" 2>&1 < /dev/null &
    QPID=$!
    local i
    for i in $(seq 1 150); do
        kill -0 "$QPID" 2>/dev/null || { tail -5 "$log"; fail "QEMU exited early ($log)"; }
        if $REMOTE run true >/dev/null 2>&1; then ok "ssh up (${i}x2s) $log"; return 0; fi
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

echo "== images"
rm -rf "$WORK"; mkdir -p "$WORK"
SRC="$FXDIR/fxgap-$ARCH.img"
[ -f "$SRC" ] || fail "missing $SRC (FX_GAP_GIB=24 scripts/make-dualboot-fixture.sh $ARCH $SRC)"
for f in "$SRC"*; do cp -c "$f" "$WORK/$(basename "$f" | sed "s/fxgap-$ARCH/disk/")"; done
cp -c "$IMG" "$WORK/pristine.img"
STICK="$WORK/stick.img"
"$ROOT/scripts/write-usb.sh" "$ARCH" --image "$STICK" --gpt >"$WORK/write-usb.log" 2>&1 || { tail -5 "$WORK/write-usb.log"; fail "write-usb.sh"; }

echo "== boot the stick with the fixture as an NVMe target"
LEANDROS_USB_IMAGE="$STICK" boot_vm stick --usb-stick=gpt --install-target="$IMG,nvme"
[ -f "$BIN" ] && { $REMOTE push "$BIN" /tmp/leandros-install >/dev/null; r 'chmod +x /tmp/leandros-install'; }
LI=/tmp/leandros-install
r "$LI list --json" > "$WORK/list.json"
python3 - "$WORK/list.json" <<'PY' || fail "list --json"
import json, sys
d = {x["name"]: x for x in (json.loads(l) for l in open(sys.argv[1]) if l.startswith("{"))}
t = d["nvme0n1"]
assert t["dualboot"] and t["gaps"], t
b = [x for x in d.values() if x["boot"]]
assert len(b) == 1 and not b[0]["dualboot"], b
g = max(t["gaps"], key=lambda g: g["sectors"])
print("ok: list: nvme0n1 dualboot=true, gap %d MiB" % (g["sectors"] // 2048))
PY
BOOTDISK="$(python3 -c "import json; print([json.loads(l)['name'] for l in open('$WORK/list.json') if l.startswith('{') and json.loads(l)['boot']][0])")"
if r "$LI install --mode dualboot --use-free --target /dev/$BOOTDISK --yes --json; true" | grep -q '"ev":"done"'; then fail "install onto the stick was accepted"; fi
ok "install onto the boot stick refused"
# Hash through /tmp files, not `dd bs=1M | sha256sum`: the host-side hash of the
# pristine image below is the ground truth for the guest's view.
snap() { r "dd if=/dev/nvme0n1 bs=1M count=2 of=/tmp/h0 2>/dev/null; sha256sum < /tmp/h0; dd if=/dev/nvme0n1 bs=1M skip=49150 of=/tmp/h1 2>/dev/null; sha256sum < /tmp/h1; rm -f /tmp/h0 /tmp/h1" | tr '\n' ' '; }
hsnap() { echo "$(dd if="$1" bs=1M count=2 2>/dev/null | shasum -a 256 | cut -d' ' -f1)  - $(dd if="$1" bs=1M skip=49150 2>/dev/null | shasum -a 256 | cut -d' ' -f1)  - "; }
B4=$(snap)
[ "$B4" = "$(hsnap "$WORK/pristine.img")" ] && ok "guest hash of the disk matches the host hash" || fail "guest disk hash differs from the host image hash"
r "$LI install --mode dualboot --use-free --target /dev/nvme0n1 --dry-run --json" > "$WORK/dry.log"
grep -q '"ev":"done","dry_run":true' "$WORK/dry.log" || { tail -3 "$WORK/dry.log"; fail "dry run did not finish"; }
grep -q '"ev":"plan".*"new_gpt"' "$WORK/dry.log" && grep -q 'EFI/leandros' "$WORK/dry.log" || fail "dry run plan lacks new_gpt/esp_files"
[ "$(snap)" = "$B4" ] && ok "dry run printed the plan and wrote nothing" || fail "dry run changed the disk"

echo "== install into the free space"
r "$LI install --mode dualboot --use-free --target /dev/nvme0n1 --yes --json" > "$WORK/install.log" || { tail -5 "$WORK/install.log"; fail "install failed"; }
DONE="$(grep '"ev":"done"' "$WORK/install.log" | tail -1)"; [ -n "$DONE" ] || { tail -3 "$WORK/install.log"; fail "no done event"; }
for s in backup gpt boot log home root verify esp; do grep -q "\"name\":\"$s\",\"pct\":100" "$WORK/install.log" || fail "step $s did not finish"; done
ok "all steps finished"
RO=$(r "lsblk 2>/dev/null | grep '^/dev/nvme0n1 ' | tr -s ' ' | cut -d' ' -f3; true"); [ "$RO" = "1" ] && ok "nvme0n1 read-only again after the install" || fail "nvme0n1 left writable after the install (RO=$RO)"
ROOT_PU=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['root_partuuid'])" "$DONE")
BK=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['backup_dir'])" "$DONE")
r "ls -l $BK; cd $BK && sha256sum -c SHA256SUMS" | tail -9
stop_vm

echo "== host checks"
"$ROOT/scripts/check-dualboot-fixture.sh" "$IMG" --installed | tee "$WORK/check.log" | grep -E "^(PASS|FAIL|RESULT)" | sort | uniq -c | sort -rn | head -40
grep -q "^RESULT installed fail=0" "$WORK/check.log" || fail "check-dualboot-fixture.sh --installed"
ok "check-dualboot-fixture.sh --installed"
P5=$(sgdisk -i 5 "$IMG" | sed -n 's/^First sector: *\([0-9]*\).*/\1/p')
python3 "$ROOT/scripts/fsck-f2fs.py" --help >/dev/null 2>&1 || true
P5E=$(sgdisk -i 5 "$IMG" | sed -n 's/^Last sector: *\([0-9]*\).*/\1/p')
python3 - "$IMG" "$P5" "$P5E" "$WORK/root.img" <<'PY'
import sys, struct
img, a, b, out = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
f = open(img, "rb"); f.seek(a * 512 + 1024); sb = f.read(100)
assert struct.unpack_from("<I", sb, 0)[0] == 0xF2F52010, "p5 is not f2fs"
blocks = struct.unpack_from("<Q", sb, 36)[0]
f.seek(a * 512)
with open(out, "wb") as o:
    o.truncate(blocks * 4096)
    for i in range(blocks):
        d = f.read(4096)
        if d.count(0) != 4096: o.seek(i * 4096); o.write(d)
PY
python3 "$ROOT/scripts/fsck-f2fs.py" "$WORK/root.img" > "$WORK/fsck.log" 2>&1 || { tail -8 "$WORK/fsck.log"; fail "fsck-f2fs.py on the new root"; }
ok "fsck-f2fs.py: $(tail -1 "$WORK/fsck.log" | cut -c1-100)"
P6=$(sgdisk -i 6 "$IMG" | sed -n 's/^First sector: *\([0-9]*\).*/\1/p')
python3 "$ROOT/scripts/mkfs-f2fs-home.py" is-home "$IMG" --offset $((P6 * 512)) >/dev/null || fail "p6 is not a leandros-home volume"
ok "p6 is a home volume"

echo "== Linux still boots by default"
"$ROOT/scripts/check-dualboot-fixture.sh" "$IMG" --boot "$ARCH" | tail -3 | grep -q "PASS boot" || fail "Linux boot (LINUX-DUALBOOT-OK) after the install"
ok "systemd-boot default boots Linux (LINUX-DUALBOOT-OK)"

echo "== choose LeandrOS (scratch copy)"
SC="$WORK/scratch.img"; cp -c "$IMG" "$SC"; for x in meta manifest esp.sha256; do cp -c "$IMG.$x" "$SC.$x"; done
ESP="$SC@@2M"
mtype -i "$ESP" ::/loader/loader.conf > "$WORK/loader.conf.orig"
printf 'default leandros.conf\ntimeout 1\nconsole-mode keep\n' > "$WORK/loader.conf.test"
mcopy -o -i "$ESP" "$WORK/loader.conf.test" ::/loader/loader.conf
boot_vm leandros --boot-disk="$SC,nvme"
CMD=$(r 'cat /proc/cmdline'); echo "$CMD" | cut -c1-260
echo "$CMD" | grep -qi "root=PARTUUID=$ROOT_PU" && ok "root=PARTUUID=$ROOT_PU" || fail "cmdline root"
echo "$CMD" | grep -q 'blk.rw=' && ok "blk.rw= on the cmdline" || fail "blk.rw= missing"
echo "$CMD" | grep -q 'nvme.rw=1' && fail "nvme.rw=1 present"
r 'cat /proc/mounts' | grep -Eq '^/dev/nvme0n1p5 / ' && ok "root is /dev/nvme0n1p5" || { r 'cat /proc/mounts' | head -5; fail "root not on p5"; }
for d in /dev/nvme0n1p2 /dev/nvme0n1p1 /dev/nvme0n1p3; do
    o=$(r "dd if=/dev/zero of=$d bs=512 count=1 conv=notrunc 2>&1; echo rc=\$?")
    echo "$o" | grep -qi 'read-only file system\|EROFS' && ok "write to $d: EROFS" || { echo "$o"; fail "write to $d was not refused"; }
done
o=$(r "dd if=/dev/zero of=/dev/nvme0n1 bs=512 count=1 seek=100 conv=notrunc 2>&1; echo rc=\$?")
echo "$o" | grep -qi 'read-only file system\|EROFS' && ok "write to the whole disk: EROFS" || { echo "$o"; fail "whole-disk write not refused"; }
r 'echo leandros-ok > /root/dbfree.txt && sync && cat /root/dbfree.txt' | grep -q leandros-ok && ok "LeandrOS root is writable"
stop_vm
mcopy -o -i "$ESP" "$WORK/loader.conf.orig" ::/loader/loader.conf

echo "== Linux and the ESP are still untouched after the LeandrOS session"
"$ROOT/scripts/check-dualboot-fixture.sh" "$SC" --installed | tee "$WORK/check2.log" | grep -E "^FAIL" || true
grep -q "^RESULT installed fail=0" "$WORK/check2.log" || fail "post-session fixture check"
ok "post-session check clean"
echo "PASS: dualboot-free-test $ARCH"
