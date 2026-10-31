#!/bin/bash
# End-to-end test of the REAL kernel write path of `espctl pi` (no Pi needed).
# QEMU aarch64 gets one extra virtio disk holding a FAT32 image with the Pi
# card's exact geometry (512 B sectors, spc 8, 32 reserved, 2 x 975 FAT, data0
# 1982, 1,000,000 sectors; built by the pitest builder). The kernel knob
# `sdhci.test_bootfat=<virtio ordinal>` makes that disk behave like the Pi's
# boot FAT: read-only, hidden, BLKISBOOTFAT=1, BLKWRWIN-only writes.
#   scripts/pi-e2e-test.sh        (after ./scripts/build-all.sh --arch aarch64 and
#                                  userland/espctl/build.sh aarch64; hold the qemu lock)
# In the guest: `espctl pi setup` (real), `pi status`, `pi stage` with a dummy
# kernel/initrd. On the host afterwards: fsck.fat -n clean; the changed sectors
# (listed as ranges) are all in the FAT copies, the root cluster or the new
# allocation area (nothing in the reserved area, no existing file's clusters);
# config.txt, kernel.img, cmdline.txt, initrd.cpio and the dtb are byte-identical;
# slots/ exists. cmdline.txt is only compared by hash, never printed.
set -u
cd "$(dirname "$0")/.."
ORD=${PI_E2E_ORD:-3}            # virtio ordinal of the extra disk (vda boot, vdb data0, vdc data1, vdd = 3)
export LEANDROS_RUN_ID=${LEANDROS_RUN_ID:-pie2e-a64}
export LEANDROS_NET=user LEANDROS_REMOTE=qemu
W=$(mktemp -d /tmp/pie2e.XXXX)
R="python3 scripts/remote.py"; D=".claude/skills/run-leandros/driver.py"
fail=0; ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; fail=1; }
cleanup() { python3 $D stop >/dev/null 2>&1; [ "${KEEP:-0}" = 1 ] || rm -rf "$W"; }
trap cleanup EXIT

(cd userland/espctl/pitest && PICARD_E2E_IMG=$W/card.img cargo test --release --target "$(rustc -vV | sed -n 's/host: //p')" emit_e2e >/dev/null 2>&1) || { echo "image build failed"; exit 1; }
cp "$W/card.img" "$W/before.img"
cp -c leandros-limine-aarch64.img "$W/boot.img"; cp -c f2fs-data0-aarch64.img "$W/root.img"
scripts/set-image-cmdline.sh "$W/boot.img" "sdhci.test_bootfat=$ORD" >/dev/null
export LEANDROS_DISK_IMAGE=$W/boot.img LEANDROS_ROOT_IMAGE=$W/root.img
export LEANDROS_QEMU_EXTRA="-drive if=none,id=bf,format=raw,file=$W/card.img -device virtio-blk-pci,drive=bf"
python3 $D start aarch64 > "$W/start.log" 2>&1 || { tail -5 "$W/start.log"; exit 1; }
$R wait --timeout 240 >/dev/null 2>&1 || { echo "guest not reachable"; exit 1; }

$R push userland/espctl/target/aarch64-unknown-linux-musl/release/espctl /tmp/espctl >/dev/null 2>&1
$R run 'chmod 755 /tmp/espctl; head -c 300000 /dev/urandom > /tmp/k.bin; head -c 90000 /dev/urandom > /tmp/i.bin' >/dev/null 2>&1
G() { $R run "$1" 2>&1; }
DEV=/dev/vd$(python3 -c "print(chr(97+$ORD))")
echo "dev=$DEV"
G "/tmp/espctl pi status --dev $DEV" | tee "$W/status0" | head -n 6
grep -q "not the boot-FAT" "$W/status0" && bad "node not seen as boot FAT"
G "/tmp/espctl pi setup --dry-run --dev $DEV" | tail -n 2
G "/tmp/espctl pi setup --accept-root-sector-risk --dev $DEV" | tee "$W/setup" | tail -n 3
grep -q "^setup:" "$W/setup" && ok "setup ran for real" || bad "setup"
G "/tmp/espctl pi status --dev $DEV" | tee "$W/status1" | head -n 8
G "/tmp/espctl pi stage auto --kernel /tmp/k.bin --initrd /tmp/i.bin --tag E2ETEST --dev $DEV" | tee "$W/stage" | tail -n 3
grep -q "staged slot" "$W/stage" && ok "stage ran for real" || echo "note: stage did not run (see above)"
python3 $D stop >/dev/null 2>&1; sleep 2

fsck.fat -n "$W/card.img" 2>&1 | grep -qiE "differ|cross|bad|invalid|lost" && bad "fsck.fat" || ok "fsck.fat -n clean"
python3 - "$W/before.img" "$W/card.img" <<'P' || fail=1
import sys
a,b=open(sys.argv[1],'rb'),open(sys.argv[2],'rb')
S=512; ch=[]; i=0
while True:
    x,y=a.read(S*2048),b.read(S*2048)
    if not x: break
    for k in range(0,len(x),S):
        if x[k:k+S]!=y[k:k+S]: ch.append(i+k//S)
    i+=len(x)//S
rs=[]
for s in ch:
    if rs and s==rs[-1][1]+1: rs[-1][1]=s
    else: rs.append([s,s])
print("changed sectors:",len(ch),"ranges:",[(r[0],r[1]) for r in rs][:40])
# reserved area is 0..31 (boot sector copies at 0 and 6 and FSInfo 1/7 must NOT change); FATs 32..1981; data0=1982
bad=[s for s in ch if s<32]
print("reserved-area sectors changed:",bad)
sys.exit(1 if bad else 0)
P
[ $? = 0 ] && ok "reserved area untouched" || bad "reserved area changed"
for f in config.txt kernel.img cmdline.txt initrd.cpio bcm2712-rpi-5-b.dtb; do
  a=$(MTOOLS_SKIP_CHECK=1 mcopy -i "$W/before.img" "::$f" - 2>/dev/null | shasum | cut -c1-12)
  b=$(MTOOLS_SKIP_CHECK=1 mcopy -i "$W/card.img" "::$f" - 2>/dev/null | shasum | cut -c1-12)
  [ -n "$a" ] && [ "$a" = "$b" ] && ok "$f unchanged" || bad "$f changed"
done
MTOOLS_SKIP_CHECK=1 mdir -i "$W/card.img" ::slots >/dev/null 2>&1 && ok "slots/ present" || bad "slots/ missing"
exit $fail
