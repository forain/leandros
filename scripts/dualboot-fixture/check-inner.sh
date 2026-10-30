#!/bin/sh
# Runs in an Alpine container. Env: MODE (pristine|installed). Image at /img.
set -u
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
if [ "$MODE" = installed ]; then
  P2E=$(field 2 "Last sector" | cut -d' ' -f1)
  for n in 4 5 6; do
    s=$(field $n "First sector" | cut -d' ' -f1); e=$(field $n "Last sector" | cut -d' ' -f1)
    if [ -z "$s" ]; then bad "p$n missing"; continue; fi
    [ "$s" -gt "$P2E" ] && [ "$e" -lt "$FX_P3_START" ] && ok "p$n [$s,$e] inside gap" || bad "p$n outside gap"
    echo "  p$n name=$(field $n 'Partition name') type=$(field $n 'Partition GUID code' | cut -c1-36) attr=$(field $n 'Attribute flags')"
  done
  for n in 4 5; do field $n "Attribute flags" | grep -qi '^8000000000000000$' && ok "p$n no-auto attr (bit 63)" || bad "p$n attr bit 63"; done
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
  ls $W/esp/EFI/leandros/BOOT*.EFI >/dev/null 2>&1 && ok "/EFI/leandros/BOOT*.EFI" || bad "/EFI/leandros/BOOT*.EFI"
  LC=$W/esp/EFI/leandros/limine.conf
  [ -f $LC ] && ok "/EFI/leandros/limine.conf" || bad "limine.conf missing"
  P4=$(field 4 "Partition unique GUID" | tr A-Z a-z)
  grep -qi "root=PARTUUID=$P4" $LC 2>/dev/null && ok "limine.conf root=PARTUUID=p4" || bad "limine.conf root"
  grep -q 'nvme.rw=1' $LC 2>/dev/null && ok "nvme.rw=1" || echo "  note: no nvme.rw=1 in limine.conf"
  for p in $(sed -n 's/^ *\(module_\)\?path: *boot():\(.*\)$/\2/p' $LC 2>/dev/null); do
    [ -e "$W/esp$p" ] && ok "path exists $p" || bad "path missing $p"
  done
  [ -f $W/esp/loader/entries/leandros.conf ] && { ok "loader/entries/leandros.conf"; sed 's/^/  | /' $W/esp/loader/entries/leandros.conf; } || bad "loader entry leandros.conf"
  grep -q '^efi */EFI/leandros/BOOT' $W/esp/loader/entries/leandros.conf 2>/dev/null && ok "entry efi path" || bad "entry efi path"
  [ -d $W/esp/leandros ] && ok "/leandros dir" || bad "/leandros dir"
  # loader.conf must be unchanged (covered by the ESP hash check above).
fi
echo "RESULT $MODE fail=$FAIL"
exit $((FAIL > 0))
