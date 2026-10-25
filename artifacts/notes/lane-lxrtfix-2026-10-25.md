# lane lxrtfix (+ lxrtfix2) — 2026-10-25 — openbugs #46 (lx volumes only)

Status: PARTIAL (see "Open").

## Commits
- `ced07c53` cp tail summary ORDER for non-umount packs (lane lxrtfix).
  Linux `sum_blk_addr`: umount pack -> data sums at total-7, node at total-4; any other pack -> data
  sums at total-4, node cursegs from the SSA area. We always wrote umount order, so after a power cut
  fsck.f2fs compared the hot-data curseg with the hot-node summary (the lxpop post-regress rc=4).
  `lx_tail_sums(ms, logs, umount)`; checker `scripts/fsck-f2fs.py` reads at the same addresses.
- `3603771c` (lane lxrtfix2):
  - (a) roll-forward `recover()` (durable.rs) now writes the SSA entry of every replayed node
    (nid, ofs 0) and data block (owner nid, slot = `alloc_data_owned` convention, NAT version) and types
    the segment's class (`lx_type_class`). Before, the summaries/types set at allocation died with the
    cache: "Invalid node seg summary", SIT/valid_node count mismatches after a reboot of a killed VM.
    `tree()` returns data as (addr, owner nid, slot).
  - (b) "Incorrect curseg [1]: segno 0x1 type(SIT) 0" / "Next block offset is not free": `linux_logs`
    fell back to segment `t` at block 0 when no empty segment was left (near-full volume). That put
    the warm-data curseg on a VALID block of a hot-data segment, and could give two heads the same
    segment (instrumented: `t=1 seg=5` then `t=5 seg=5`). Heads are SSR already, so now: a free
    block anywhere in our current segment (t=0/3), else `lx_ssr_head` (segment of type t, then of
    t's class, then a pre-free one; "free" = clear in the SIT this checkpoint writes, both bit
    orders), else a segment no other head has. `log_alloc` reaches the four filler heads only after
    SSR (`sit_find_free_seg(.., heads)`, `ssr_pick(.., heads)`); legacy volumes unchanged.

## Evidence
- `F2FS_TEST_LX=1 f2fscrashtest.py aarch64 --iters 2 --mode kill --seed 1792903575`
  - baseline (main + ced07c53): "fsck after recovery + umount" FAIL both iters
    ("curseg 1 segment 1: SIT type 0 != 1", "curseg 0 segment 0: SIT type 3 != 0").
  - fixed: "fsck after recovery + umount" PASS both iters.
  - "fsck of the crashed image" FAILS on baseline AND fixed — see Open.
- Fresh LEANDROS_F2FS_LX=1 build (data0 image deleted first) -> full aarch64 regress PASS (boots failed 0/1);
  that image: Mac fsck-f2fs PASS, box fsck.f2fs rc=0 (only cosmetic `-f` i_gc_failures resets).
- Same image booted once more and SIGKILLed (checkpoint ver 17, NOT clean): Mac PASS, box rc=0.
- f2fscrashtest legacy (same seed) PASS 2/2 incl. crashed-image fsck; f2fscptest lx and legacy PASS
  (the earlier hang at 'Waiting for login' did not reproduce; a concurrent run from the same worktree
  fails exactly that way because QEMU locks the shared images, likely the earlier cause).

## Open
1. Full lx volume (crashtest's 64 MiB volume reaches ENOSPC: "out of space: no usable block left"):
   the crashed image's cursegs have no valid position — every segment is full or of the wrong
   class (instrumented: `head t=5 seg=5 type=0 vb=512`), and valid_user_blocks exceeds
   user_block_count ("Wrong valid_user_blocks: 11320, user_block_count: 11264"; the Linux kernel
   refuses such a checkpoint). Pre-existing (identical on the baseline). Fix needs a Linux-style
   reserve on lx volumes: cap valid blocks at user_block_count and keep the four filler heads (+1
   spare per class) strictly out of our allocator, i.e. ENOSPC earlier. Design decision for the
   orchestrator (capacity cost ~4-6 segments).
