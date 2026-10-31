# lane/pistragglerfix 2026-10-31

Changes (commit 8c20da9e):
- sched/src/kthread.rs: `masked_poll_point()`; `park_check` in `sleep_until`'s irq_window branch.
- drivers/src/brcmf/netdev.rs: `service()` calls `masked_poll_point()`; sdio_host.rs: `park_check` in bare waits.
- kernel/src/kexec/exec_aarch64.rs: `kexec.test_straggler_mode=busy` (spin::Mutex held, IRQs masked, busy pass).
- kexecfdttest.py: busy mode on gen7 plus a gen8 -> gen9 leg.
- b7555db9: `set_rw_list` stub in blkdev_sdhci.rs (rpi5 kernel did not compile since the dbinteg merge; lane/pistragglerdiag has the same stub, expect a trivial merge conflict).

Tests (aarch64, hvf):
- `runtests.py aarch64 <tag> --suite kexecfdt` (a bare tag runs the whole suite instead): PASS, lock, rq and busy legs.
- Fail side: busy_pass with `cond_resched()` instead of `masked_poll_point()`: `gen8 -> gen9 (busy straggler)` FAILs. Restored.
- Full-suite run (before the suite fix) only failed pre-existing nettest tcp_keepalive_probes and netctltest wired_admin_up_rebinds.
- Boot smoke covered by runtests boots.

Pi 5 image: `build-all.sh --rpi5`, target/final-aarch64/kernel-direct tag b7555db9, CONFIG_BOARD_RPI5=y.

## Real Pi 5 result 2026-10-31
- good ab90917bd8 → trial b7555db96b: straggler, reset to the card kernel (expected: the unfixed kernel did the kexec).
- card → trial b7555db96b: ok.
- 5/5 trial→trial kexec rounds out of b7555db96b: ok, every round `[KEXEC] park stages: cpu1=4 cpu2=4 cpu3=4`
  (Wi-Fi up, brcmf active). Before the fix: 2/3–6/6 rounds aborted with the straggler at stage=0.
- trial → good (6th kexec out of the fix): ok, back on ab90917bd8 in 21 s.
- Diagnostic lane A was not needed: the fix result confirms H1 (brcmf service loop under spin::Mutex with no park point).
