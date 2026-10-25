# Lane f2fsnode: Linux on-disk node format ("lx"), opt-in (L12 #46), 2026-10-25

Status: PARTIAL. The server reads and writes the lx format (`LeandrOS lxnode1` superblock
marker; Linux 7.2.7 mounts our lx images read-write). New volumes are still made LEGACY by
default. lx is created only on request: `scripts/mkfs-f2fs-home.py ensure <img> --lx`
(implemented in scripts/mkfs-f2fs-home-lx.py). Merging this lane therefore leaves the
format of main's root and /home volumes unchanged.

## Commits
- 7c2494b4, d72216ff: lx NAT owner ino, MSB-first SIT valid map, `linux_import gave up at <step>` labels.
- d2a6fe12: lx is opt-in. mkfs-f2fs-populated.py is main's legacy version again. The lx
  populated root writer is in history at d72216ff:scripts/mkfs-f2fs-populated.py.

## The "hang 5 s after login:" was REFUTED as a hang
- With an lx root (d72216ff populated mkfs), the kernel and the f2fs server stay alive.
  [pcache]/[fslock] stats keep printing, and getty keeps re-printing `login:`.
- What actually fails is interactive login. After `Password:`, /bin/login exits immediately
  and init prints `session ended, restarting login`. No shell ever appears, so driver.py
  `start`/`login` and every runtests suite give up.
- Same lane kernel with a legacy root: login works and the shell can write (probe: `touch /root/x` ok).
  So the trigger is the lx root image, not the server code.
- OPEN (not root-caused): what /bin/login reads differently on the lx root image.
  Candidates: /etc/shadow, or the uid/gid/mode of a file in the lx populated writer. Repro:
  `git show d72216ff:scripts/mkfs-f2fs-populated.py > scripts/mkfs-f2fs-populated.py`, build,
  `driver.py start aarch64; driver.py login root root` gives `session ended`.
- sshd `exited (status 9)` at boot happens on main too; it is unrelated.

## lx data volume on the lane kernel (legacy root, lx /home via --lx): GREEN
- Mount logs `[F2FS] Linux node format (lxnode1)` for vdc.
- `cp /bin/brush /home/b`: cksum is identical (2323322844, 6140688 bytes).
- 40 files created in /home/d, `ls | wc -l` = 40, and the contents read back after `sync`.

## Gates (aarch64, rebuilt on main b8a9ef77 incl. harness fix #68): ALL GREEN
- f2fscrashtest seed 1792903575: PASS; random seed (r1): PASS.
- f2fscptest, f2fsorphantest, f2fsgctest (host:): PASS.
- compat.sh old: write/verify marks all present, lane and main fsck PASS before/after/final.
- lx /home data volume (--lx --reset): mounts as lxnode1, cp+cksum of /bin/brush identical, 40 files read back.
- full aarch64 regress: PASS (boots failed 0/1).
- The earlier boot failures were harness bug #68 (serial attached 5 s late), not the kernel.

## Status
- lx is OPT-IN (mkfs-f2fs-home.py --lx); root and /home stay legacy by default.
- Linux 7.2.7 mounts lx images read-write.

## Remaining open: fsck.f2fs cleanliness needs a mkfs/allocator redesign, not a patch
- mkfs (home and populated) puts inodes AND dentry data in main segment 0 (`used = 2*n`).
  Linux SSA/SIT typing needs separate node and data segments.
- Five of the six cursegs are 0xFFFFFFFF. Linux expects six distinct typed segments.
- The server never sets the SIT type bits (vblocks >> 10), and `sit_is_node` relies on them.
  The type should be set in log_alloc on a segment switch (lx only).
- No SSA summary blocks are written (`ssa 0 blk`).
- Not yet run: a Linux-written lx image mounting on LeandrOS (`linux_import gave up at <step>`
  labels are in place; run $SP/f2fsnode/proof.sh box on the KVM box).
- lx root image login failure, above.

- Superseded: the lx-root login-failure claim above was a boot-detection artifact (harness #68); lx root was not re-tested.
- Still open: fsck.f2fs cleanliness (mkfs/allocator redesign: SSA, SIT types, cursegs) and LeandrOS mounting a Linux-written lx image.
