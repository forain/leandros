# lane/buddyslack 2026-11-11 - REFUTED (already fixed + guarded)

Both halves of the TODO item were closed earlier:
- Eager->lazy split slack: fixed 2026-09-24 (split_at/clone_as give back the buddy tail); guard memtest `eager_split_frees_tail`.
- "~27 pages/death lazy-fault residual": refuted by lane/leakres (2026-10-23, <0.01 page/death); guard memtest `death_residual`.

Re-measured on current main (962fa702, aarch64 HVF, `/bin/memtest`, fails=0):
- eager_split_frees_tail: 64 rounds, lost_pages=0 (bar 512; unfixed leak = 63/round)
- death_residual: batch 1 lz/true/kill all lost_pages=0, per_death_x100=0 (batch 0 lz: 16 pages/200 deaths, one-time warm-up)
- exit_frees_page_tables: 32 deaths, 0 KiB/death

No code change; TODO item struck. No mm change, so no x86 box regress needed (memtest is in the regress list on both arches).
