# lane/pollout — 2026-09-18

## Item 1: `polltest pipe_epoll_pollout_reflects_ring_full` — kernel is RIGHT, test was wrong

Root cause: `d7caad8` (lane/idlecpu) changed the pipe write end's POLLOUT rule to
Linux's real contract — writable only once **>= PIPE_BUF (4096) bytes are free**,
not merely `count < capacity`. `pipe_epoll_pollout_reflects_ring_full` in
`userland/polltest/src/main.rs` predates that change: it fills the ring to EAGAIN,
drains only 256 bytes, and asserted POLLOUT should reappear. 256 free bytes is far
under the 4096-byte PIPE_BUF threshold, so on the new (correct) rule it correctly
stays not-writable, and the test's assertion was the stale side.

Verified against real Linux, not from memory, as instructed: ssh'd to the desktop
(172.16.158.150) and ran a `python3`/`os.pipe`+`fcntl(O_NONBLOCK)`+`select.poll`
script that fills a stock 65536-byte Linux pipe to EAGAIN, then reads back byte
counts one at a time around the boundary:
- free=4095 bytes → `poll()` reports NOT writable.
- free=4096 bytes (one more byte read) → `poll()` reports writable.

That is an exact match for our kernel's `PIPE_RING_SIZE - r.count >= PIPE_BUF`
rule in `servers/vfs/src/lib.rs` (~line 4061). The kernel change in `d7caad8` is
correct; the test was the regression.

Fix: `userland/polltest/src/main.rs`, `test_pipe_epoll_pollout_reflects_ring_full`
(around line 237). Now does two drains instead of one:
1. Drain 256 bytes → assert epoll_wait still reports NOT writable (new negative
   check, guards the exact bug this test used to have backwards).
2. Drain the rest of a full PIPE_BUF (4096) cumulative → assert epoll_wait now
   reports EPOLLOUT.

Comment added citing the exact Linux ground-truth measurement above (free=4095
not writable, free=4096 writable) so the threshold isn't "wrong" again by
someone reading only the kernel source.

TODO.md: checked every `polltest 6/6` (and similar count) mention — all are
historical snapshots from earlier milestones (`m7-progress.md`,
`m9-todo-reconciled.md`, old TODO.md sections) that were accurate for their own
point in time; the *number* of subtests in `polltest` did not change (still 6),
only subtest #2's internal assertions. No stale count found that needed editing,
and no section documents `d7caad8`/lane/idlecpu as having verified `polltest`
passing (so nothing there needed correcting either). Left TODO.md untouched.

## Item 2: doomgeneric shared-tree build collision — NOT DONE, dropped for budget

Confirmed the bug: `scripts/build-all.sh`'s `build_doom()` runs
`make -f Makefile.leandros ARCH=$arch ... clean` in the single shared sibling
`/Users/forain/code/doomgeneric` (not a git repo; `../doomgeneric` resolves to
the same physical directory from every worktree since they share `~/code/` as
parent). `Makefile.leandros`'s `clean` target is `rm -rf x86_64 aarch64
doom-x86_64 doom-aarch64` — wipes BOTH arches' object dirs and BOTH final
binaries unconditionally, so two worktrees building concurrently (even different
arches) race: one's `clean` can delete the other's in-progress `.o` files or its
finished `doom-$arch` output out from under `mkfs-f2fs-populated.py`, which reads
it from a fixed relative path (`../doomgeneric/doom-{arch}`) that is also shared
across worktrees.

Planned fix (not implemented, not tested — do this next):
1. Add `OBJDIR ?= $(ARCH)` to `Makefile.leandros`, use it everywhere `$(ARCH)/`
   currently prefixes object paths; `build-all.sh` passes a per-worktree,
   per-arch value, e.g. `OBJDIR=$doom_dir/.obj-$(basename "$ROOT_DIR")-$arch`.
2. Drop the `make ... clean` call in `build_doom()` entirely — per-worktree
   OBJDIR isolation means no cross-worktree staleness risk, and within one
   worktree the source never changes mid-wave, so incremental rebuild is safe
   and strictly faster.
3. Make the final link step atomic against concurrent readers: link to
   `doom-$(ARCH).tmp.$$$$` then `mv -f` onto `doom-$(ARCH)` (mv is atomic on the
   same filesystem), since the final binary name/location is still necessarily
   shared (mkfs's relative-path lookup is fixed).
4. Prove it with two concurrent `build-all.sh` runs from two worktrees (this one
   + a throwaway third worktree, NOT leandros-integ), then remove the throwaway.

I did not touch `/Users/forain/code/doomgeneric/Makefile.leandros` or
`scripts/build-all.sh` at all — zero risk to the other lanes currently building
against that shared sibling. Whoever picks this up next can start clean from the
plan above.

## Verification status — READ CAREFULLY, branch is WIP re: full verification

- `userland/polltest` (the only change on this branch) compiles cleanly:
  `cargo build -p polltest --target aarch64-unknown-none --release` — success,
  1 pre-existing warning (`pid_t` unused, not from this change).
- Ground truth for the fix (real Linux POLLOUT/PIPE_BUF boundary) was measured
  live over ssh, see above — that part IS solid.
- **NOT DONE**: a full `./scripts/build-all.sh`, QEMU boot, and actual
  `polltest` run on either aarch64 (HVF) or x86_64 (TCG) inside LeandrOS. Ran
  out of budget before getting a kernel image built. This is a single, small,
  self-contained userland test-file change with no kernel-side edit, so risk is
  low, but it is NOT proven booted per lane protocol — next session must run
  `polltest` on both arches before calling this mergeable.

## Files touched

- `userland/polltest/src/main.rs` (test fix, item 1) — only file changed.
- `artifacts/notes/lane-pollout-2026-09-18.md` (this note).

No kernel files touched. No sibling repos touched. No stray QEMUs were started
by this lane (none were run at all). No temporary worktree was created (item 2
was dropped before reaching the "prove it" step, so the throwaway worktree the
plan describes was never made).

## MERGEABLE status: **WIP-only, not verified booted**

The diff is small and self-contained (userland test file only) and compiles,
but per lane protocol "every fix ships with a regression test ... and is
verified booted on BOTH arches" — that boot verification did not happen this
session. Do not merge without running `polltest` on both arches first.
