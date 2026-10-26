# lxmigrate2 S0 (guest): stage the lx root on sda4 @0 and @8GiB. Usage: sh s0v2.sh <img> <nz 4M blocks> <full 4M blocks> <sha256 of full>
img=$1; nz=$2; full=$3; want=$4
fail() { echo "S0 FAIL: $*"; exit 1; }
echo "S0 start $(date)"
touch /etc/leandros/text-login
for i in 1 2 3 4 5; do
  ps | while read pid tty tm cmd; do
    case $cmd in greetd|cosmic-*|greeter*|pipewire*|wireplumber|Xwayland) kill -9 $pid 2>/dev/null;; esac
  done
  sleep 3
  umount /home 2>/dev/null && break
done
case "$(df /home | tail -n 1)" in *sda4*) fail "home still mounted";; esac
case "$(df / | tail -n 1)" in *sda4*) fail "root is sda4";; esac
echo "home unmounted"
for base in 0 2048; do
  dd if=$img of=/dev/sda4 bs=4M seek=$base conv=fsync 2>&1 | tail -n 1
  dd if=/dev/zero of=/dev/sda4 bs=4M seek=$((base + nz)) count=$((full - nz)) conv=fsync 2>&1 | tail -n 1
  echo "written base $base $(date)"
done
sync
h0=$(dd if=/dev/sda4 bs=4M count=$full iflag=fullblock 2>/dev/null | sha256sum | cut -d' ' -f1)
h8=$(dd if=/dev/sda4 bs=4M skip=2048 count=$full iflag=fullblock 2>/dev/null | sha256sum | cut -d' ' -f1)
[ "$h0" = "$want" ] || fail "readback @0 $h0"
[ "$h8" = "$want" ] || fail "readback @8G $h8"
echo "readback ok @0 and @8G"
mkdir -p /mnt/new
mount -t f2fs /dev/sda4 /mnt/new || fail "mount sda4"
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
echo "S0 OK $(date)"
