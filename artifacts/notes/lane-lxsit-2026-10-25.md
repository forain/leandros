# Lane lxsit: SIT types, stable cursegs, checkpoint counters on lx volumes (L2 of design f2fsfsck, openbugs #46), 2026-10-25

Status: FIXED for E3a/b/c, E4, E7, E8 (lx volumes only; legacy paths untouched). SSA/owner summaries remain for L3 lxssa.

Changes (servers/f2fs/src/lib.rs, durable.rs; all gated on `fmt.lx`)
- `sit_set_type(ms, seg, t)`: writes SIT bits 10-15 and keeps `valid_nodes` = sum of node-typed segments.
- `log_alloc` types the new segment (3 node log, 0 data log) whichever way it was chosen (free or SSR).
- `ssr_pick(ms, node)`: on lx only segments of the same class (or empty) are reused.
- `linux_logs` keeps the previous filler pick while it is still empty (`CpInfo.logs`, parsed from the cp at mount),
  and types all six heads; `sit_find_free_seg` and `pick_ext` avoid them (first pass; a second pass allows them when
  nothing else is free, so small volumes still fill up).
- `lx_cp_prep` runs at the start of `cp_prepare` / `cp_emergency` (before the dirty set is collected, so the type
  changes ride in that checkpoint) and recounts `free_segment_count` (empty segments that are not a head).
- `valid_inode_count` (CP @148) kept in `CpInfo.valid_inodes`: ++ in `create_inode`, -- in `nat_clear` of an inode
  entry, written by `linux_cp_fields`. (Counted from the cp at mount; inodes recreated by crash roll-forward are not
  counted, so it can drift after a crash until a fresh mkfs; a mount-time NAT scan would fix it.)
- Cosmetic: klog text and scripts/fsck-f2fs.py docstring no longer say lxnode1.
- Test scripts f2fscptest/f2fscrashtest/f2fsfulltest take `F2FS_TEST_LX=1` to run on an lx volume.

Evidence (box fsck.f2fs 1.16.0 --dry-run, 64 MiB lx image, design work.py workload on aarch64, rc stays 255)
- Gone: all `Incorrect curseg ... type`, `ino[0x1]/[0x2] block_addr invalid`, valid_node_count (nat lookup) now OK
  [0xd0], free segment_count OK [0xc].
- Remaining (L3 lxssa): `summary node block is not valid [0x3]` / `Invalid node seg summary` (no SSA), which stops the
  walk, hence unreachable NIDs (0xcf), valid_block_count / valid_node_count (de lookup) / valid_inode_count (walk
  based, 0), SIT valid bitmap checking, "fixing SIT types".
- aarch64 regress PASS; f2fscptest and f2fscrashtest (seed 1792903575) PASS on lx; legacy variants run too.
