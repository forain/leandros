# lane/podclean 2026-11-11 -- FIXED

- Rootless pasta churn measured: 40x `podman run --rm true` as uid 1000 (su -l leandro), aarch64 HVF, fails=0; no pasta left, no netns, no mount above baseline.
- Found: podpastatest ran as root (harness shell) so it never tested rootless; it now runs every command via `su -l leandro -c`.
- Removed redundant `tunbe::local_loop` (subsumed by `loop_own`).
- podpastatest: portfwd uses an nc loop in the container (Alpine has no httpd); netns mount leak check compares against a baseline count; netns dir check ignores `ls` headers. Same baseline fix in podbridgetest.
- sched/ns.rs: slot classes split: net 3, ipc 7, uts 8 (`new_flat(owner, class)`, `link_class`), so hold_checked on a net nsfs source no longer accepts an ipc/uts slot.
- Open: none from this row. Box regress results in the merge message / status line.
