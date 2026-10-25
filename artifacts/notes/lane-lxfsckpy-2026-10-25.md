# Lane lxfsckpy: host checker lx rules + SSA rebuild tool (L5 of design f2fsfsck, E11), 2026-10-25

Status: FIXED (scripts only; servers/f2fs untouched).

- `scripts/fsck-f2fs.py`: on lx volumes it now also checks SIT type per segment class (no mixed node/data segment, curseg t has
  SIT type t), SSA owner entry (nid, ofs; data version = NAT version) and footer byte for every reachable block, the cp-tail
  curseg summaries (footer and owners below the curseg offset), NAT entries for ino 1/2, and cp valid_node_count,
  valid_inode_count, valid_block_count, free_segment_count. Failures are errors (`lx: ...`). Legacy is unchanged.
- `scripts/f2fs-lx-ssa-rebuild.py IMG [--check]`: from ssafix.py; rebuilds SSA, curseg summaries, SIT types, NAT ino 1/2, cp
  counters (Linux CRC, both pack ends). Refuses mixed segments, legacy volumes, a checkpoint with a journal.
- `scripts/f2fs-lx-corrupt.py IMG ssa|sit`: corruption helper for the guard.

Evidence (64 MiB lx image, aarch64 QEMU, design work.py workload; box fsck.f2fs 1.16.0 --dry-run)
- Clean image: ours PASS, box rc=0 (0 ASSERT). Also fresh mkfs image: PASS.
- One SSA data entry corrupted: ours FAIL (1 error, names segment/offset), box rc=4 (7 matching lines).
- Warm-node curseg SIT type zeroed: ours FAIL (`curseg 4 ... SIT type 0 != 4`), box rc=4 (`Incorrect curseg [4] type(SIT) [0]`).
- Rebuild tool on both: ours PASS and box rc=0, 0 ASSERT.
- Note: fsck.f2fs does not flag a wrong SIT type on a non-curseg segment (it derives the type itself); ours does, deliberately.
- Node SSA versions are not compared (mkfs writes 1, rebuild writes 0; fsck.f2fs ignores them).
