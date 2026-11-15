# lane/f2fsnlog0 (design-f2fsnodelog P0) - 2026-11-15 - DELIVERED

Done
- Checkpoint flag constants fixed (servers/f2fs/src/cpflags.rs): NOCRC 0x200, LARGE_NAT_BITMAP 0x400, QUOTA_NEED_FSCK 0x800.
  Consumers: linux_import (large NAT bail), RO/fsck mount decision, linux_chain (NOCRC compares low 32 bits; only FSYNC 0x2 marks a
  node, review L4), and a large-NAT-bitmap checkpoint now mounts read-only explicitly (review L5). Volumes we wrote carry only flags 0/1
  (+0x40 later), so behaviour for them is unchanged. Host tests: `cd servers/f2fs-tests && cargo test --target aarch64-apple-darwin` (9 pass).
- scripts/f2fslx/chaindump.py: Linux-exact warm-chain walk (CRC/NOCRC), I1,I3,I4,I7,I8 always; --strict adds I2,I5,I6. Validated against
  a Linux-made crash image: same 4 nodes as fsck.f2fs -d 1 (do_record_fsync_data), no violations; --strict flags it (Linux head is not (S,0)).
- Interop direction E: `interop.sh fsynce` (F2FSINTEROP_E="name ..." selects scenarios). 10 scenarios, steps E.chain/cponly/fsck0/linux/back/ours.
- Gate: box regress aarch64 86/0 and x86_64 PASS on ce8288e9 (kernel identical to final tree; later commits are python/TODO only).

Red baseline on main (interop-known-failures.txt): E.chain and E.fsck0 fail for append,newfile,inline,overwrite,indirect,xattr,ab,era,full
(marked=0, 0 node records); E.linux fails for append,newfile,indirect,ab,era (content lost on Linux). E.ours passes everywhere (LRF1).
rename passes all (checkpoint fallback).

Findings for P1
- E.linux ALREADY PASSES for inline, overwrite, xattr, full on today's main although E.cponly says a norecovery mount misses the content
  (contradiction: unexplained; either in-place data + something, or E.cponly vacuous). Investigate first in P1.
- Several scenarios end with newest CP flagged UMOUNT (cp_umount=1; Linux skips roll-forward then): P1 must ensure the era CP is not an
  umount CP (mount must commit a non-umount CP before the first chained fsync).
- No sed/awk in the guest; fsync a file with `sync <file>`.
