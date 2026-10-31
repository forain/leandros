#!/bin/sh
# Runs in an Alpine container. Env: MODE (pristine|installed). Image at /img.
set -u
[ -f /img.meta ] && . /img.meta
. /scripts/constants.sh
apk add -q e2fsprogs e2fsprogs-extra dosfstools mtools sgdisk util-linux coreutils >/dev/null
export MTOOLS_SKIP_CHECK=1
IMG=/img; FAIL=0; W=/tmp/chk; mkdir -p $W
ok() { echo "PASS $*"; }
bad() { echo "FAIL $*"; FAIL=$((FAIL+1)); }
chk() { d=$1; shift; if "$@" >/dev/null 2>&1; then ok "$d"; else bad "$d"; fi; }
[ -f $IMG.meta ] && . $IMG.meta
pi() { sgdisk -i "$1" $IMG 2>/dev/null; }
field() { pi "$1" | sed -n "s/^$2: *//p" | head -1; }

echo "== GPT"
sgdisk -v $IMG | grep -q "No problems found" && ok "sgdisk -v clean" || { bad "sgdisk -v"; sgdisk -v $IMG | head -5; }
sgdisk -p $IMG | sed -n '/^Number/,$p'
[ "$(sgdisk -p $IMG | sed -n 's/^Disk identifier (GUID): //p' | tr A-Z a-z)" = "$FX_DISK_GUID" ] && ok "disk GUID" || bad "disk GUID"
for spec in "1 $FX_ESP_START $((FX_ESP_START+FX_ESP_SECTORS-1)) $FX_P1_GUID" \
            "3 $FX_P3_START $FX_P3_END $FX_P3_GUID" "2 $FX_P2_START $FX_P2_END $FX_P2_GUID"; do
  set -- $spec; n=$1
  s=$(field $n "First sector" | cut -d' ' -f1); e=$(field $n "Last sector" | cut -d' ' -f1)
  g=$(field $n "Partition unique GUID" | tr A-Z a-z)
  [ "$s" = "$2" ] && ok "p$n start $s" || bad "p$n start $s != $2"
  [ "$g" = "$4" ] && ok "p$n GUID" || bad "p$n GUID"
  if [ "$n" = 2 ] && [ "$MODE" = installed ]; then
    [ "$e" -le "$3" ] && ok "p2 end $e <= original $3" || bad "p2 end grew"
  else
    [ "$e" = "$3" ] && ok "p$n end $e" || bad "p$n end $e != $3"
  fi
done
[ "$(field 2 'Partition name')" = "'endeavouros'" ] && ok "p2 name" || bad "p2 name"
BOOTT=0E02ED9A-FEEE-4CAB-A22F-C861AD7DE307; ROOTT=3F2D991B-D385-4FFF-9CF1-A0DB03062DE5
HOMET=F936DEE1-99D9-4BBC-8D2A-B7447DB9F650; LOGT=3EC8FA02-816A-4D04-8718-64C3771C5187
if [ "$MODE" = installed ]; then
  P2E=$(field 2 "Last sector" | cut -d' ' -f1)
  for spec in "4 $BOOTT" "5 $ROOTT" "6 $HOMET" "7 $LOGT"; do
    set -- $spec; n=$1
    s=$(field $n "First sector" | cut -d' ' -f1); e=$(field $n "Last sector" | cut -d' ' -f1)
    if [ -z "$s" ]; then bad "p$n missing"; continue; fi
    [ "$s" -gt "$P2E" ] && [ "$e" -lt "$FX_P3_START" ] && ok "p$n [$s,$e] inside gap" || bad "p$n outside gap"
    [ $((s % 2048)) = 0 ] && ok "p$n 1 MiB aligned" || bad "p$n not aligned"
    [ "$(field $n 'Partition GUID code' | cut -c1-36 | tr a-z A-Z)" = "$2" ] && ok "p$n type $2" || bad "p$n type"
    field $n "Attribute flags" | grep -qi '^8000000000000000$' && ok "p$n no-auto attr (bit 63)" || bad "p$n attr bit 63"
    echo "  p$n name=$(field $n 'Partition name')"
  done
  [ -z "$(field 8 'First sector')" ] && ok "no p8" || bad "unexpected p8"
else
  [ -z "$(field 4 'First sector')" ] && ok "no p4 yet" || bad "unexpected p4"
fi

echo "== ext4 p2"
P2S=$(field 2 "First sector" | cut -d' ' -f1); P2OFF=$((P2S * 512)); E="$IMG?offset=$P2OFF"
e2fsck -fn "$E" >$W/e2fsck.log 2>&1 && ok "e2fsck -fn clean" || { bad "e2fsck -fn"; tail -3 $W/e2fsck.log; }
BLK=$(dumpe2fs -h "$E" 2>/dev/null | sed -n 's/^Block count: *//p')
if [ "$MODE" = pristine ]; then [ "$BLK" = "$FX_P2_BLOCKS" ] && ok "block count $BLK" || bad "block count $BLK"
else echo "  block count $BLK ($((BLK * 4 / 1024 / 1024)) GiB)"; fi
sb() { dd if=$IMG bs=1 skip=$((P2OFF + 1024 + $1)) count=4 2>/dev/null | od -An -tx4 | tr -d ' '; }
[ "$(sb 92)-$(sb 96)-$(sb 100)" = "0000103c-000022c2-0000046b" ] && ok "feature bits = laptop" || bad "feature bits $(sb 92)-$(sb 96)-$(sb 100)"
[ "$(dumpe2fs -h "$E" 2>/dev/null | sed -n 's/^Filesystem UUID: *//p')" = "$FX_P2_UUID" ] && ok "fs UUID" || bad "fs UUID"
[ -f $IMG.manifest ] && MAN=$IMG.manifest || MAN=""
debugfs -R "rdump /testdata $W" "$E" >/dev/null 2>&1
if [ -n "$MAN" ]; then cp $MAN $W/MANIFEST.sha256; else debugfs -R "cat /MANIFEST.sha256" "$E" > $W/MANIFEST.sha256 2>/dev/null; fi
if [ -z "$MAN" ]; then echo "  (no host manifest sidecar; using the on-disk one)"; fi
(cd $W && sha256sum -c MANIFEST.sha256 2>&1 | grep -vc ': OK$' > nbad.txt; true)
NB=$(cat $W/nbad.txt); NF=$(wc -l < $W/MANIFEST.sha256)
[ "$NB" = 0 ] && ok "sha256 ($NF files) all match" || bad "sha256 mismatches: $NB of $NF"
[ -z "$MAN" ] || { debugfs -R "cat /MANIFEST.sha256" "$E" 2>/dev/null | cmp -s - $MAN && ok "on-disk MANIFEST intact" || bad "on-disk MANIFEST differs"; }

echo "== swap p3"
if [ -n "${FX_SWAP_SHA256:-}" ]; then
  S=$(field 3 "First sector" | cut -d' ' -f1)
  H=$(dd if=$IMG bs=512 skip=$S count=$FX_SWAP_SECTORS 2>/dev/null | sha256sum | cut -d' ' -f1)
  [ "$H" = "$FX_SWAP_SHA256" ] && ok "swap sha256 unchanged" || bad "swap sha256 changed"
else echo "  SKIP (no meta sidecar)"; fi

echo "== ESP p1"
dd if=$IMG of=$W/esp.img bs=512 skip=$FX_ESP_START count=$FX_ESP_SECTORS conv=sparse 2>/dev/null
fsck.fat -n $W/esp.img >$W/fsckfat.log 2>&1 && ok "fsck.fat -n clean" || { bad "fsck.fat"; tail -3 $W/fsckfat.log; }
mdir -i $W/esp.img -/ -b ::/ 2>/dev/null | sed 's/^/  /'
mkdir -p $W/esp; mcopy -i $W/esp.img -s -n ::/* $W/esp/ 2>/dev/null
if [ -f $IMG.esp.sha256 ]; then
  (cd $W/esp && sha256sum -c $IMG.esp.sha256 2>&1 | grep -vc ': OK$' > $W/nesp.txt; true)
  [ "$(cat $W/nesp.txt)" = 0 ] && ok "every original ESP file unchanged" || { bad "ESP files changed/missing"; (cd $W/esp && sha256sum -c $IMG.esp.sha256 2>&1 | grep -v ': OK$' | head -5); }
  if [ "$MODE" = pristine ]; then
    (cd $W/esp && find . -type f | sort) | diff -q - <(cut -c67- $IMG.esp.sha256 | sort) >/dev/null 2>&1 && ok "ESP file list exact" || true
  fi
else echo "  SKIP hash check (no sidecar)"; fi
EFIU=$(echo ${FX_EFI:-x64} | tr a-z A-Z)
[ -f $W/esp/EFI/systemd/systemd-boot${FX_EFI:-x64}.efi ] && ok "systemd-boot present" || bad "systemd-boot missing"
cmp -s $W/esp/EFI/systemd/systemd-boot${FX_EFI:-x64}.efi $W/esp/EFI/BOOT/BOOT$EFIU.EFI && ok "BOOT$EFIU.EFI = systemd-boot" || bad "BOOT$EFIU.EFI differs"
ls $W/esp/loader/entries/${FX_MACHINE_ID}-*.conf >/dev/null 2>&1 && ok "Linux BLS entry" || bad "Linux BLS entry"

if [ "$MODE" = installed ]; then
  echo "== LeandrOS side"
  P4U=$(field 4 "Partition unique GUID" | tr A-Z a-z); P5U=$(field 5 "Partition unique GUID" | tr A-Z a-z)
  P6U=$(field 6 "Partition unique GUID" | tr A-Z a-z); P7U=$(field 7 "Partition unique GUID" | tr A-Z a-z)
  EN=BOOT$EFIU.EFI
  [ -s $W/esp/EFI/leandros/$EN ] && ok "/EFI/leandros/$EN" || bad "/EFI/leandros/$EN"
  LC=$W/esp/EFI/leandros/limine.conf
  grep -q 'protocol: efi' $LC 2>/dev/null && grep -qi "path: guid($P4U):/EFI/BOOT/$EN" $LC && ok "chain conf -> boot partition $P4U" || bad "chain conf"
  [ "$(cat $W/esp/EFI/leandros/.leandros-owned 2>/dev/null)" = "$P4U" ] && ok "ESP owner marker" || bad "ESP owner marker"
  EC=$W/esp/loader/entries/leandros.conf
  [ -f $EC ] && { ok "loader/entries/leandros.conf"; sed 's/^/  | /' $EC; } || bad "loader entry leandros.conf"
  grep -q "^efi */EFI/leandros/$EN" $EC 2>/dev/null && grep -q '^title LeandrOS' $EC && grep -q '^sort-key zz-leandros' $EC && ok "entry fields" || bad "entry fields"
  N=$(cd $W/esp && find . -type f | wc -l); N0=$(wc -l < $IMG.esp.sha256)
  [ "$N" = "$((N0 + 4))" ] && ok "ESP gained exactly 4 files" || bad "ESP file count $N (was $N0)"
  # The private boot partition.
  S4=$(field 4 "First sector" | cut -d' ' -f1); E4=$(field 4 "Last sector" | cut -d' ' -f1)
  dd if=$IMG of=$W/boot.img bs=512 skip=$S4 count=$((E4 - S4 + 1)) conv=sparse 2>/dev/null
  fsck.fat -n $W/boot.img >/dev/null 2>&1 && ok "boot partition fsck.fat clean" || bad "boot partition fsck.fat"
  mkdir -p $W/bp; mcopy -i $W/boot.img -s -n ::/* $W/bp/ 2>/dev/null
  grep -q "^leandros-boot $P4U" $W/bp/leandros/owner 2>/dev/null && ok "boot partition owner marker" || bad "boot partition owner marker"
  BC=$W/bp/limine.conf
  grep -qi "root=PARTUUID=$P5U" $BC 2>/dev/null && ok "root=PARTUUID=<p5>" || bad "boot limine.conf root"
  for u in $P5U $P6U $P7U $P4U; do grep -qi "blk.rw=.*$u" $BC && ok "blk.rw has $u" || bad "blk.rw lacks $u"; done
  grep -q 'nvme.rw=1' $BC && bad "nvme.rw=1 present" || ok "no nvme.rw=1"
  for p in $(sed -n 's/^ *\(module_\)\?path: *boot():\(.*\)$/\2/p' $BC 2>/dev/null); do
    [ -e "$W/bp$p" ] && ok "boot path exists $p" || bad "boot path missing $p"
  done
  [ -s $W/bp/EFI/BOOT/$EN ] && ok "inner Limine EFI/BOOT/$EN" || bad "inner Limine"
fi
echo "RESULT $MODE fail=$FAIL"
exit $((FAIL > 0))
