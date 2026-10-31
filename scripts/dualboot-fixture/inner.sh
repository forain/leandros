#!/bin/sh
# Runs inside an Alpine container. Env: ARCH (x86_64|aarch64), CACHE (/cache),
# OUTDIR (/out). Builds $OUTDIR/fixture.img (+ sidecars).
set -eu
. /scripts/constants.sh
case "$ARCH" in
  x86_64)  EFI=x64; CONS=ttyS0 ;;
  aarch64) EFI=aa64; CONS=ttyAMA0 ;;
  *) echo "bad arch" >&2; exit 2 ;;
esac
W=/work; IMG=$W/disk.img
rm -rf $W; mkdir -p $W/k $W/root $W/data $W/initrd
apk add -q e2fsprogs e2fsprogs-extra dosfstools mtools sgdisk util-linux openssl \
  coreutils gzip cpio >/dev/null
export MTOOLS_SKIP_CHECK=1

echo "== fetch Alpine kernel/busybox ($ARCH)"
REPO=https://dl-cdn.alpinelinux.org/alpine/v3.21
apk --root $W/k --arch $ARCH --initdb --no-scripts --allow-untrusted --no-cache \
  -X $REPO/main -X $REPO/community add -q linux-virt busybox-static >/dev/null
KVER=$(ls $W/k/lib/modules | head -1)
VMLINUZ=$(ls $W/k/boot/vmlinuz-virt)
echo "kernel $KVER: $(file -b $VMLINUZ 2>/dev/null | cut -c1-80 || true)"
if [ ! -f $CACHE/alpine-minirootfs-$ARCH.tar.gz ]; then
  REL=$(wget -qO- $REPO/releases/$ARCH/latest-releases.yaml | awk '/file: alpine-minirootfs/{print $2; exit}')
  wget -qO $CACHE/alpine-minirootfs-$ARCH.tar.gz $REPO/releases/$ARCH/$REL
fi
tar -xzf $CACHE/alpine-minirootfs-$ARCH.tar.gz -C $W/root
mkdir -p $W/root/boot $W/root/home $W/root/data
echo endeavouros-fixture > $W/root/etc/hostname
printf 'UUID=%s / ext4 defaults 0 1\nUUID=%s none swap defaults 0 0\n' \
  $FX_P2_UUID $FX_SWAP_UUID > $W/root/etc/fstab

echo "== test data (${FX_DATA_MIB} MiB)"
rnd() { # rnd <seed> <bytes>
  openssl enc -aes-128-ctr -K "$(printf '%s' "$1" | sha256sum | cut -c1-32)" \
    -iv 00000000000000000000000000000000 < /dev/zero 2>/dev/null | head -c "$2"
}
D=$W/data; mkdir -p $D/testdata/docs/nested/deeper $D/testdata/media $D/testdata/bin
n=0; mk() { rnd "$1" "$2" > "$D/testdata/$1"; n=$((n+1)); }
mk docs/tiny.txt 1024
mk docs/odd.bin 4097
mk docs/nested/a.bin 1048576
mk docs/nested/deeper/b.bin 33554433
mk bin/tool.sh 12345
big=$(( (FX_DATA_MIB - 40) / 4 ))
for i in 1 2 3 4; do mk media/big$i.bin $((big * 1048576)); done
# sparse file: 64 MiB apparent, data only at start/middle/end
sp=$D/testdata/media/sparse.img
truncate -s 64M $sp
rnd sp1 8192 | dd of=$sp conv=notrunc 2>/dev/null
rnd sp2 8192 | dd of=$sp bs=1M seek=32 conv=notrunc 2>/dev/null
rnd sp3 4096 | dd of=$sp bs=4096 seek=16383 conv=notrunc 2>/dev/null
ln -s docs/nested/a.bin $D/testdata/link-to-a
(cd $D && find testdata -type f | sort | xargs sha256sum) > $W/MANIFEST.body
# the hardlink resolves to the same content as docs/tiny.txt
echo "$(sha256sum < $D/testdata/docs/tiny.txt | cut -d' ' -f1)  testdata/hardlink-tiny" >> $W/MANIFEST.body
echo "$(sha256sum < $D/testdata/docs/nested/a.bin | cut -d' ' -f1)  testdata/link-to-a" >> $W/MANIFEST.body
sort -k2 $W/MANIFEST.body > $W/MANIFEST.sha256
FILES=$(wc -l < $W/MANIFEST.sha256)

echo "== disk + GPT"
truncate -s $((FX_DISK_SECTORS * 512)) $IMG
sgdisk -a 1 -U $FX_DISK_GUID \
  -n 1:$FX_ESP_START:$((FX_ESP_START + FX_ESP_SECTORS - 1)) -t 1:EF00 -c 1:"EFI system partition" -u 1:$FX_P1_GUID \
  -n 2:$FX_P2_START:$FX_P2_END -t 2:8300 -c 2:endeavouros -u 2:$FX_P2_GUID \
  -n 3:$FX_P3_START:$FX_P3_END -t 3:8200 -c 3:swap -u 3:$FX_P3_GUID $IMG >/dev/null
sgdisk -v $IMG | grep -q "No problems" || { echo GPT-BAD; exit 1; }

echo "== ext4 (${FX_P2_BLOCKS} blocks) with ${FX_FILLER_GIB} GiB filler"
P2OFF=$((FX_P2_START * 512)); E="$IMG?offset=$P2OFF"
mkdir -p $W/rootfs; cp -a $W/root/. $W/rootfs/
rnd filler $((FX_FILLER_GIB * 1073741824)) > $W/rootfs/filler
mke2fs -q -t ext4 -b 4096 -L $FX_P2_LABEL -U $FX_P2_UUID \
  -O metadata_csum,metadata_csum_seed,64bit,orphan_file,huge_file,dir_nlink,extra_isize \
  -E offset=$P2OFF,nodiscard,lazy_itable_init=1,lazy_journal_init=1 -d $W/rootfs $IMG $FX_P2_BLOCKS
rm -rf $W/rootfs
{
  echo "mkdir /testdata"; echo "mkdir /testdata/docs"; echo "mkdir /testdata/docs/nested"
  echo "mkdir /testdata/docs/nested/deeper"; echo "mkdir /testdata/media"; echo "mkdir /testdata/bin"
  for f in $(cd $D && find testdata -type f); do echo "write $D/$f /$f"; done
  echo "symlink /testdata/link-to-a docs/nested/a.bin"
  echo "ln /testdata/docs/tiny.txt /testdata/hardlink-tiny"
  echo "sif /testdata/bin/tool.sh mode 0100755"; echo "sif /testdata/bin/tool.sh uid 1000"
  echo "sif /testdata/bin/tool.sh gid 1000"
  echo "sif /testdata/docs/tiny.txt mode 0100600"; echo "sif /testdata/docs/tiny.txt uid 1000"
  echo "write $W/MANIFEST.sha256 /MANIFEST.sha256"
  echo "rm /filler"
} > $W/dbg.cmd
debugfs -w -f $W/dbg.cmd "$E" >$W/dbg.log 2>&1 || { tail -5 $W/dbg.log; exit 1; }
e2fsck -fy "$E" >$W/fsck.log 2>&1 || { [ $? -le 1 ] || { tail $W/fsck.log; exit 1; }; }
e2fsck -fn "$E" | tail -1

echo "== verify feature bits equal the laptop's (compat 0x103c incompat 0x22c2 ro 0x46b)"
sb() { dd if=$IMG bs=1 skip=$((P2OFF + 1024 + $1)) count=4 2>/dev/null | od -An -tx4 | tr -d ' '; }
C=$(sb 92); I=$(sb 96); R=$(sb 100)
echo "compat=$C incompat=$I ro_compat=$R"
[ "$C" = 0000103c ] && [ "$I" = 000022c2 ] && [ "$R" = 0000046b ] || { echo "FEATURE MISMATCH"; exit 1; }

echo "== placement: lowest data block of testdata files"
MINB=$(debugfs -R "blocks /testdata/media/big1.bin" "$E" 2>/dev/null | tr -s " " "\n" | grep -E "^[0-9]+$" | head -1)
echo "big1 first physical block: ${MINB:-?} (shrink point 9G = $((9*262144)))"
[ -n "${FX_NOPLACE:-}" ] || [ "${MINB:-0}" -gt $((9*262144)) ] || { echo "PLACEMENT FAIL: data is not past the shrink point"; exit 1; }

echo "== punch holes over free ext4 blocks"
dumpe2fs "$E" 2>/dev/null | awk '/^ *Free blocks:/ {sub(/^ *Free blocks: */,""); gsub(/,/," "); print}' | tr ' ' '\n' | \
 awk 'NF && $0 ~ /^[0-9]/ {split($0,a,"-"); s=a[1]; e=(a[2]==""?a[1]:a[2]); print s, e-s+1}' | \
 while read s l; do fallocate -p -o $((P2OFF + s * 4096)) -l $((l * 4096)) $IMG; done
du -k --apparent-size $IMG | cut -f1; du -k $IMG | cut -f1

echo "== swap"
truncate -s $((FX_SWAP_SECTORS * 512)) $W/swap.img
mkswap -q -U $FX_SWAP_UUID -L swap $W/swap.img
dd if=$W/swap.img of=$IMG bs=512 seek=$FX_P3_START count=8 conv=notrunc 2>/dev/null

echo "== ESP (systemd-boot $EFI, kernel $KVER)"
ESPOFF=$((FX_ESP_START * 512))
mkfs.fat -F 32 -n ESP -i $FX_ESP_VOLID -C $W/esp.img $((FX_ESP_SECTORS / 2)) >/dev/null
M="-i $W/esp.img"
mmd $M ::/EFI ::/EFI/systemd ::/EFI/BOOT ::/loader ::/loader/entries ::/$FX_MACHINE_ID ::/$FX_MACHINE_ID/$KVER
SBE=$CACHE/systemd-boot$EFI.efi
EFIUP=$(echo $EFI | tr a-z A-Z)
mcopy $M $SBE ::/EFI/systemd/systemd-boot$EFI.efi
mcopy $M $SBE ::/EFI/BOOT/BOOT$EFIUP.EFI
printf 'timeout 3\nconsole-mode keep\n' > $W/loader.conf
mcopy $M $W/loader.conf ::/loader/loader.conf
cp $VMLINUZ $W/linux
OPTS="root=UUID=$FX_P2_UUID rw console=$CONS"
printf 'title EndeavourOS\nsort-key endeavouros\nversion %s\nlinux /%s/%s/linux\ninitrd /%s/%s/initrd\noptions %s\n' \
  $KVER $FX_MACHINE_ID $KVER $FX_MACHINE_ID $KVER "$OPTS" > $W/entry.conf
mcopy $M $W/entry.conf ::/loader/entries/$FX_MACHINE_ID-$KVER.conf
mcopy $M $W/linux ::/$FX_MACHINE_ID/$KVER/linux

echo "== initramfs"
I=$W/initrd; mkdir -p $I/bin $I/dev $I/proc $I/sys $I/mnt $I/lib/modules/$KVER
cp $W/k/bin/busybox.static $I/bin/busybox
MD=$W/k/lib/modules/$KVER
want="virtio_blk virtio_pci virtio_mmio nvme ext4 jbd2 mbcache crc32c_generic libcrc32c crc16 crc32c-intel"
sed 's/\.ko\.\(gz\|zst\|xz\)/.ko/g' $MD/modules.dep > $W/deps.txt
need=""; add() { case " $need " in *" $1 "*) return;; esac; need="$need $1"
  for d in $(grep -E "^$1:" $W/deps.txt | cut -d: -f2); do add $d; done; }
for m in $want; do
  p=$(grep -E "/${m}\.ko:" $W/deps.txt | cut -d: -f1 | head -1); [ -n "$p" ] && add "$p" || true
done
for p in $need; do
  mkdir -p $I/lib/modules/$KVER/$(dirname $p)
  src=$MD/$p; [ -f $src ] || { src=$(ls $MD/$p.* | head -1); }
  case $src in *.gz) gunzip -c $src ;; *.zst) apk add -q zstd >/dev/null; zstd -dc $src ;; *) cat $src ;; esac > $I/lib/modules/$KVER/$p
done
grep -E "^($(echo $need | tr ' ' '|' | sed 's/\./\\./g')):" $W/deps.txt > $I/lib/modules/$KVER/modules.dep || true
cp $MD/modules.builtin $MD/modules.alias $I/lib/modules/$KVER/ 2>/dev/null || true
: > $I/lib/modules/$KVER/modules.order
cat > $I/init <<INIT
#!/bin/busybox sh
/bin/busybox --install -s /bin
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sysfs /sys; mount -t devtmpfs dev /dev
for m in virtio_pci virtio_mmio virtio_blk nvme crc32c_generic ext4; do modprobe \$m || echo "[fixture-init] modprobe \$m failed"; done
ROOT=\$(sed -n 's/.*root=\([^ ]*\).*/\1/p' /proc/cmdline)
i=0; DEV=""
while [ \$i -lt 50 ]; do
  case "\$ROOT" in UUID=*|LABEL=*) DEV=\$(findfs "\$ROOT" 2>/dev/null);; *) DEV=\$ROOT;; esac
  [ -n "\$DEV" ] && [ -b "\$DEV" ] && break
  DEV=""; sleep 0.2; i=\$((i+1))
done
echo "[fixture-init] root=\$ROOT dev=\$DEV"
if [ -z "\$DEV" ] || ! mount -t ext4 -o rw "\$DEV" /mnt; then echo "LINUX-DUALBOOT-FAIL mount"; cat /proc/partitions; ls /dev | head -40; cat /proc/modules; exec sh; fi
: > /mnt/.rwtest && rm -f /mnt/.rwtest && echo "[fixture-init] root mounted read-write"
cd /mnt
FILES=\$(wc -l < MANIFEST.sha256)
BAD=\$(sha256sum -c MANIFEST.sha256 2>&1 | grep -vc ': OK\$')
PARTS=\$(grep -cE '(vd[a-z]|nvme[0-9]n[0-9]p?|sd[a-z])[0-9]+\$' /proc/partitions)
echo "LINUX-DUALBOOT-OK files=\$FILES bad=\$BAD parts=\$PARTS"
cd /; sync; umount /mnt
[ -e /noexit ] || poweroff -f
exec sh
INIT
chmod +x $I/init
(cd $I && find . | cpio -o -H newc -R 0:0 2>/dev/null | gzip -9) > $W/initrd.img
mcopy $M $W/initrd.img ::/$FX_MACHINE_ID/$KVER/initrd

dd if=$W/esp.img of=$IMG bs=512 seek=$FX_ESP_START conv=notrunc,sparse 2>/dev/null

echo "== sidecars"
O=$OUTDIR; mkdir -p $O
{ echo "FX_KVER=$KVER"; echo "FX_FILES=$FILES"; echo "FX_ARCH=$ARCH"; echo "FX_EFI=$EFI"; echo "FX_GAP_GIB=$FX_GAP_GIB"
  echo "FX_SWAP_SHA256=$(dd if=$IMG bs=512 skip=$FX_P3_START count=$FX_SWAP_SECTORS 2>/dev/null | sha256sum | cut -d' ' -f1)"
} > $O/fixture.meta
cp $W/MANIFEST.sha256 $O/fixture.manifest
# sha256 of every ESP file
mdir $M -/ -b ::/ 2>/dev/null | sed 's/^:://' >/dev/null || true
mcopy $M -s -n ::/ $W/espdump 2>/dev/null || { mkdir -p $W/espdump; mcopy $M -s ::/* $W/espdump/; }
(cd $W/espdump && find . -type f | sort | xargs sha256sum) > $O/fixture.esp.sha256
cp --sparse=always $IMG $O/fixture.img
ls -ls $O/fixture.img
echo "FIXTURE-BUILD-OK"
