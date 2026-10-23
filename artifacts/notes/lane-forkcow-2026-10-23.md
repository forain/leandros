# lane/forkcow (L10.12 remainder) - REFUTED

Question: does fork's clone_as copy the `written` set of aliased private tmpfs VMAs into the child?
Answer: yes. mm/src/cow.rs clones `written: region.written.clone()` in both lazy branches (the eager branch has no CoW'd pages to track).
Evidence: new memtest case big_private_fork_truncate (300 MiB tmpfs MAP_PRIVATE; parent CoWs P, fork, child CoWs Q, parent truncates below both):
child keeps Q and inherited P, untouched page past EOF is SIGBUS. PASS on unmodified main (aarch64); full regress PASS.
No kernel change; only the regression guard was added.
