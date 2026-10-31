# lane/pitrykern 2026-10-31 (L1) -- STATUS: built + QEMU-tested (aarch64); Pi 5 hardware untested

- sys_reboot RESTART2 "tryboot" token (kernel/src/power.rs): SET_REBOOT_FLAGS=1, read back, NOTIFY_REBOOT, then restart. Mailbox failure -> EIO (no reboot); non-rpi5 -> EOPNOTSUPP.
- Plain restarts in a trial boot clear the flag first (tryboot::before_plain_reset). Panic and watchdog-reset paths also clear it (rpi_misc::clear_trial_flag_nonblocking: once, try_lock, bounded mailbox spin).
- kernel/src/tryboot.rs: `leandros.tryboot=1` (word match; cmdline never printed) arms a deadline (tryboot.deadline=<s>, default 900). Disarm: reboot(TBOK 0x54424F4B); hold: reboot(TBHD 0x54424844, arg 1/0), 300 s lease (commit in progress).
- userland/poweroff: `reboot tryboot`, `tryboot-commit`, `tryboot-hold`, `tryboot-release`.
- drivers/src/sdhci.rs: MBR p1 (type 0b/0c, 4K-aligned) -> /dev/vdc, bounded by length, always read_only; only BLKWRWIN windows can write it. Writes are sector-granular: read, compare, skip unchanged sectors (M1). vdb math unchanged. Nothing probes/fs-scans vdc (info() skips has_f2fs).
- vdc is DiskDesc.special: hidden from /dev and /sys/class/block listings (installer, disks-rs, espctl enumeration); an explicit open of /dev/vdc still works.
- Tests (aarch64 QEMU virt): boot smoke ok; leandros.tryboot=1 tryboot.deadline=10 fires a plain reboot; tryboot-hold pauses it past the deadline (95 s); tryboot-commit + release: no fire; `reboot tryboot` -> EOPNOTSUPP, no reboot; runtests --suite regress: 63 commands, overall PASS.
- Fixed a main breakage found on the way: blkdev_sdhci lacked set_rw_list (rpi5 build failed since dbinteg); added a no-op.
- Remaining: Pi 5 hardware test (real firmware tryboot.txt flow, flag clear on panic/wdt).
