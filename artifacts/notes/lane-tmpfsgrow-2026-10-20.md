# lane/tmpfsgrow (L5.8): growable tmpfs pool

TmpPool per-slot arrays are heap Vecs: empty at boot, 512 slots on first claim, doubling to 32768
(servers/vfs/src/tmppool.rs `grow_to`/`next_cap`). TMP_VMOS grows ahead of the pool in
`tmp_free_slot` (servers/vfs/src/lib.rs; lock order TMP_FILES -> TMP_VMOS). Slot indices never move.
Ceiling 32768: largest power of two fitting u16 slot links and the 16-bit VMO id slot field.
TMP_FDREFS / TMP_INFLIGHT stay static at the ceiling.

One site beyond the design: the boot-time seed `tmp.claim(0, "/run/user/0")` indexed an empty pool and
panicked (tmppool.rs release); it now calls `tmp_free_slot` first to grow the pool.

Tests (aarch64): procmax `tmpfiles` (new, 6000 files x 2 rounds) PASS; vfstest PASS; --suite regress PASS.
Docs: no docs page stated the 4096 tmpfs limit, nothing to update.
