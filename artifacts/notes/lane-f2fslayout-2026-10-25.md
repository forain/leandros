# Lane f2fslayout (L12 #46 remainder), 2026-10-25

Scope: f2fs on-disk layout steps toward Linux interop, made safe for existing volumes. Full Linux
mountability is NOT in this lane (see "Remaining Linux-compat work").

## FIXED / DONE
- d49cfaf9: mkfs (home/populated/minimal) writes a mirrored SIT (segment_count_sit 2, two copies),
  segment_count counted from segment0_blkaddr (removes Linux "Fix alignment start(3584) end(16896)"),
  Linux-sized cp sit/nat bitmaps, CP_OWN_VER stamp (u64 @2048). servers/f2fs detects a foreign
  (Linux-written) checkpoint on 2-SIT volumes and imports it (copy-1 SIT/NAT blocks per bitmap bit, NAT
  journal + SIT journal or compacted summaries), then writes its own checkpoint. ro or unknown-feature
  plus foreign => mount refused. One-SIT volumes take the unchanged path.
- f2fscptest / f2fsorphantest / f2fsgctest "shell prompt not seen after ~8 s" under `runtests host:`:
  NOT a regression. Each test boots its own VM (run id `<caller>-f2fs*`) on the same root image while
  runtests' VM still holds it, so QEMU exits at once with `Failed to get "write" lock`. Main (a35fc9b5)
  fails identically (RC=1, 8.1 s); direct invocation passes on both. Fix: the three tests stop the
  caller's VM first (as blkslowtest does); driver.py now prints QEMU's stderr when QEMU exits before
  the prompt, so this shows as what it is. After the fix: all three PASS via runtests host: (40.7/39.8/11.9 s).

## Evidence (aarch64, HVF, lane kernel)
- Old-layout image (main's mkfs, segment_count_sit 1): mount, write 1500-block file + 10 files, umount,
  remount md5 OK; second boot remount + write + remount md5 OK (2/2 files); fsck-f2fs PASS before, after
  write, after verify, with both lane and main fsck-f2fs (ver 7, leaked 0). No Linux-import line.
- New-layout image (segment_count_sit 2): identical sequence, identical results, fsck PASS both tools.
- f2fscrashtest seeds 1792903575, 1792914281: PASS both, 0 "imported a Linux" lines (no false foreign detection).
- Full aarch64 regress (tag f2fsl2reg2): overall PASS, boots failed 0/1. (A first run failed to boot only because a concurrent crash-test VM held the same worktree image: never run two VMs from one worktree at once.)
- Synthetic Linux checkpoint (prior session, $SP/f2fslayout/fakelinux.py): "[F2FS] imported a Linux
  checkpoint: 28", md5 OK, remount OK, fsck PASS (fsck had FAILed the image before the mount).
- Linux 7.2.7 (box): "Wrong bitmap size" and "Fix alignment" are gone.

## Remaining Linux-compat work (later lane)
1. Node footer: ours is at 4076 (nid@4076, ino@4080, ... 5 x u32); Linux's struct node_footer is at
   4072 (nid u32, ino u32, flag u32, cp_ver u64, next_blkaddr u32 @4092). Consequence: our direct node
   holds 1019 addresses / indirect 1019 nids, Linux ADDRS_PER_BLOCK = NIDS_PER_BLOCK = 1018. Linux mount
   currently fails at the root inode: `inconsistent node block, nid:3, node_footer[nid:0,ino:3,ofs:0,
   cpver:0,blkaddr:1]`. Needs a per-volume node format (gate on segment_count_sit >= 2): footer offset
   4072, 1018 addrs per dnode, cp_ver in the footer; sites: NODE_FOOTER_OFF in the 3 mkfs scripts,
   scripts/fsck-f2fs.py (~:37, :347), servers/f2fs lib.rs (~38 sites), durable.rs (3, incl. PER =
   NODE_FOOTER_OFF / 4), populated.py:2723 ("4076/4 = 1019 slots").
2. Inline features: the kernel implements no inline_data / inline_dentry / inline_xattr. Linux must
   mount with noinline_data,noinline_dentry,noinline_xattr; Linux-written volumes with inline inodes are
   unreadable by us. Either implement them or refuse/flag such inodes.
3. fsck.f2fs "Wrong layout: check mkfs.f2fs version" persists (rsvd/ovp (1,1),(1,2) tried; it prints
   `MKFS version ""`). Not the fsmeta/ovp/rsvd rule. Next: read f2fs-tools fsck/mount.c
   sanity_check_ckpt + the sb version/init_version checks on the box instead of guessing.
