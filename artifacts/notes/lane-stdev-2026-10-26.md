# lane/stdev — 2026-10-26 (openbugs #90, P2) — FIXED

**Bug:** `write_stat_full_rdev` hardcoded `st_dev = 1` for every filesystem, so two f2fs
volumes cloned from one image gave identical (st_dev, st_ino): `cp -a /a/f /b/f` failed
"are the same file"; -xdev walks, du, tar, rsync, git misjudged mount boundaries/hard links.

**Fix (servers/vfs/src/lib.rs):**
- `MountEntry.dev`: set in `register_mount` to the backing block device's dev_t
  (`block::lookup_dev_node` + `dev_rdev`, e.g. vdd = 254:48), else anonymous `0:(32+slot)`;
  never shared by two live mounts.
- Mount-proxied `stat`/`lstat`/`fstat` replies get `st_dev` stamped from the mount
  (`set_stat_dev`), so statx `stx_dev_*` follows too.
- tmpfs roots /tmp, /dev/shm, /run/user: anonymous 0:10, 0:11, 0:12 (path stat and fstat).
  Everything else served by the VFS itself (initrd, ramfs, /proc, /dev) stays 0:1.
- `/proc/self/mountinfo` prints each mount's real major:minor (was `0:<mount_id>`) and the
  tmpfs roots' minors, so it agrees with stat.

**Guard:** `.claude/skills/run-leandros/stdevtest.py` (`runtests.py aarch64 <tag> --suite stdev`
or `host:stdevtest.py`). Two extra virtio disks cloned from one f2fs image.
Old code: 6/9 FAIL (st_dev 1/1, cp "are the same file", tmpfs 1 1 1, mountinfo 0:N).
New code: 9/9 ok (st_dev 65072 vs 65088 = 254:48/254:64 = rdev of vdd/vde, cp -a ok,
tmpfs 65040/10/11, mountinfo agrees). aarch64 regress: PASS.

**Open:** x86_64 not run (per-lane rule). statfs `f_fsid` still not per-mount.
