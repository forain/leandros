# lxmigrate2 S1 (guest, root = sda4): copy the pristine image (sda4 @8GiB) to sda2. Usage: sh s1v2.sh <full 4M blocks> <sha256>
full=$1; want=$2
fail() { echo "S1 FAIL: $*"; exit 1; }
echo "S1 start $(date)"
case "$(df / | tail -n 1)" in *sda4*) ;; *) fail "root is not sda4";; esac
case "$(df -a 2>/dev/null | tail -n +2)" in *sda2*) fail "sda2 mounted";; esac
dd if=/dev/sda4 of=/dev/sda2 bs=4M skip=2048 count=$full iflag=fullblock conv=fsync 2>&1 | tail -n 1
sync
h=$(dd if=/dev/sda2 bs=4M count=$full iflag=fullblock 2>/dev/null | sha256sum | cut -d' ' -f1)
[ "$h" = "$want" ] || fail "readback sda2 $h"
echo "readback ok sda2"
mkdir -p /mnt/new
mount -t f2fs /dev/sda2 /mnt/new || fail "mount sda2"
mkdir -p /mnt/new/etc/leandros/network /mnt/new/etc/ssh /mnt/new/root/.ssh
cp -a /etc/leandros/wifi.conf /mnt/new/etc/leandros/ || fail cp1
cp -a /etc/leandros/network/. /mnt/new/etc/leandros/network/ || fail cp2
cp -a /etc/ssh/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key.pub /mnt/new/etc/ssh/ || fail cp3
cp -a /root/.ssh/authorized_keys /mnt/new/root/.ssh/ || fail cp4
chmod 700 /mnt/new/root/.ssh
for f in etc/leandros/wifi.conf etc/ssh/ssh_host_ed25519_key etc/ssh/ssh_host_ed25519_key.pub root/.ssh/authorized_keys; do
  a=$(sha256sum < /$f); b=$(sha256sum < /mnt/new/$f)
  [ "$a" = "$b" ] && echo "same $f" || fail "copy differs $f"
done
touch /mnt/new/etc/leandros/text-login
sync
umount /mnt/new || fail "umount new"
echo "S1 OK $(date)"
