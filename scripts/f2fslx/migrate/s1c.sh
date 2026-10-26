# lxmigrate3 S1 completion (guest, root = sda4): finish the secrets copy to sda2 by content (cp -a falsely says "same file":
# both f2fs mounts report st_dev 1). Never prints file contents.
fail() { echo "S1C FAIL: $*"; exit 1; }
case "$(df / | tail -n 1)" in *sda4*) ;; *) fail "root is not sda4";; esac
case "$(cat /proc/mounts)" in *"/mnt/new"*) ;; *) mkdir -p /mnt/new; mount -t f2fs /dev/sda2 /mnt/new || fail "mount sda2";; esac
case "$(cat /proc/mounts)" in *"sda2 /mnt/new"*) ;; *) fail "sda2 not on /mnt/new";; esac
mkdir -p /mnt/new/etc/leandros/network /mnt/new/etc/ssh /mnt/new/root/.ssh
cpf() { # cpf <path> <mode> <owner:group>
  cat "/$1" > "/mnt/new/$1.tmp" || fail "write $1"; chmod $2 "/mnt/new/$1.tmp"; chown $3 "/mnt/new/$1.tmp"
  mv -f "/mnt/new/$1.tmp" "/mnt/new/$1" || fail "mv $1"
  a=$(sha256sum < "/$1"); b=$(sha256sum < "/mnt/new/$1"); [ "$a" = "$b" ] && echo "same $1" || fail "differs $1"; }
cpf etc/leandros/wifi.conf "$(stat -c %a /etc/leandros/wifi.conf)" "$(stat -c %u:%g /etc/leandros/wifi.conf)"
for f in /etc/leandros/network/*; do p=${f#/}; cpf $p "$(stat -c %a $f)" "$(stat -c %u:%g $f)"; done
chown "$(stat -c %u:%g /etc/leandros/network)" /mnt/new/etc/leandros/network; chmod "$(stat -c %a /etc/leandros/network)" /mnt/new/etc/leandros/network
for p in etc/ssh/ssh_host_ed25519_key etc/ssh/ssh_host_ed25519_key.pub root/.ssh/authorized_keys; do cpf $p "$(stat -c %a /$p)" "$(stat -c %u:%g /$p)"; done
chmod 700 /mnt/new/root/.ssh
touch /mnt/new/etc/leandros/text-login
ls -ln /mnt/new/etc/leandros/network /mnt/new/etc/leandros/wifi.conf /mnt/new/etc/ssh/ssh_host_ed25519_key | cut -c1-60
sync; umount /mnt/new || fail "umount new"
echo "S1C OK"
