# lxmigrate3 S3 (guest, root = sda2): write the fresh lx /home head to sda4, verify by readback.
fail() { echo "S3 FAIL: $*"; exit 1; }
echo "S3 start $(date)"
case "$(df / | tail -n 1)" in *sda2*) ;; *) fail "root is not sda2";; esac
case "$(cat /proc/mounts)" in *sda4*) fail "sda4 mounted";; esac
n=$(( $(stat -c %s /tmp/home-head2.img) / 4096 ))
[ "$(sha256sum < /tmp/home-head2.img | cut -d' ' -f1)" = "2d1317d8eae58d61422ed13b3de98c1fe7df82d65dd52782a9361c94609f1444" ] || fail "src sha"
dd if=/tmp/home-head2.img of=/dev/sda4 bs=4M conv=fsync 2>&1 | tail -n 1
sync
h=$(dd if=/dev/sda4 bs=4096 count=$n iflag=fullblock 2>/dev/null | sha256sum | cut -d' ' -f1)
[ "$h" = "2d1317d8eae58d61422ed13b3de98c1fe7df82d65dd52782a9361c94609f1444" ] || fail "readback $h"
echo "S3 OK $(date)"
