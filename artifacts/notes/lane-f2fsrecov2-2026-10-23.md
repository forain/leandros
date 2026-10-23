# lane/f2fsrecov2 2026-10-23 - near-full crash recovery (L4.11)

FIXED (continues lane/f2fsrecov, c46922ef): roll-forward replays nodes/NAT, then SIT, then names, so
restoring a name can no longer allocate a block a later record owns (double ownership, garbled dir block).

REFUTED (the "second bug", seed 5 iter 3 loses one acked file per run): not a filesystem bug. The
crashed image (/tmp/f2ffull-aarch64-f2fsrecov-aarch64/crashed-iter3.img, cp ver 282) holds both
dentries (w3/s1004 -> ino 3956, w3/s966 -> ino 4533) and none of the 129 roll-forward records touches
their nids, homes or the w3 dir blocks. The serial log shows the scan line split by a kernel message:
`P s966[IRQ] line 0x00000023 fired 1000 times with no handler claiming it; masked` / ` OK`, so the host
regex missed it. f2fsfulltest now drops kernel log lines (whole or spliced) before parsing (verified on
that log: s966 recovered, no other key lost).

Test harness: fsck of the recovered tree now runs BEFORE the test directory is removed.

Evidence (aarch64): regress full PASS (incl. pollnarrowtest); f2fstest PASS; f2fscrashtest --iters 6
--seed 11 PASS; f2fsfulltest --iters 5 --seed 5 PASS (all iters recovered, fsck of recovered tree clean,
leaked 0).
Open: guard seeds 11/23 + a second seed-5 run with the new harness not yet run. New: aarch64 IRQ line
0x23 fires unclaimed 1000x and gets masked during f2fs churn (which device?).
