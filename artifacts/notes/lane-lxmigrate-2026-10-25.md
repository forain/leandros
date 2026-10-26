# lane/lxmigrate 2026-10-25: laptop stick f2fs volumes to lx -- PARTIAL

- /home (sda4, 43719327744 B): FIXED. Wrote the mkfs-f2fs-home.py `head` image for the partition size, mounted: klog
  `[F2FS] Linux node format (lxnode)`, clean checkpoint (ver 4) after a reboot, rw write+sync+readback ok, scripted desktop login up
  in ~6 s as uid 1000. Old /home contents scrapped (user decision). Good kernel 17c93ea8cfbd unchanged, ESP untouched.
- Root (sda2): NOT migrated, still the legacy volume. Plan: stage the new root on sda4 (@0 bootable copy with wifi.conf/network/ssh
  host keys copied guest-side, @8 GiB pristine copy), boot a trial with root=...-04, promote, dd the pristine copy to sda2, trial on
  -02, promote, re-seed /home. Blocked: the 2 GB `dd ... conv=fsync` to sda4 died silently twice (dd and shell gone, no klog line);
  a 98 MB `oflag=dsync` write works. Also lost one attempt to the Mac sleeping (ssh-bound run; detach with nohup < /dev/null).
- Caveat: factory a3f226cc has no lx reader, so once root is lx the factory entry cannot mount root.
