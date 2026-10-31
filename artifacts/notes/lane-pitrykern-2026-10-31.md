# lane/pitrykern 2026-10-31 (L1) -- STATUS: code written, NOT build-verified or tested

- sys_reboot RESTART2 "tryboot" token (kernel/src/power.rs): SET_REBOOT_FLAGS=1, read back, NOTIFY_REBOOT, then restart. Mailbox failure -> EIO (no reboot); non-rpi5 -> EOPNOTSUPP.
- Plain restarts in a trial boot clear the flag first (tryboot::before_plain_reset). Panic/watchdog path NOT covered (flag is one-shot).
- kernel/src/tryboot.rs: `leandros.tryboot=1` (word match; cmdline never printed) arms a deadline (tryboot.deadline=<s>, default 900). Disarm: reboot(TBOK 0x54424F4B); hold: reboot(TBHD 0x54424844, arg 1/0), 300 s lease (commit in progress).
- userland/poweroff: `reboot tryboot`, `tryboot-commit`, `tryboot-hold`, `tryboot-release`.
- drivers/src/sdhci.rs: MBR p1 (type 0b/0c, 4K-aligned) -> /dev/vdc, bounded by length, always read_only; only BLKWRWIN windows can write it. Writes are sector-granular: read, compare, skip unchanged sectors (M1). vdb math unchanged. Nothing probes/fs-scans vdc (info() skips has_f2fs).
- Caveat: vdc is enumerated as a normal disk by the VFS block layer; installer/disks-rs/espctl repair-esp would see it on a Pi.
- TODO: aarch64 build + QEMU smoke, deadline fire/commit test (tryboot.deadline=10 via scripts/set-image-cmdline.sh), RESTART2 tryboot EOPNOTSUPP test on virt, --rpi5 build + tag/CONFIG_BOARD_RPI5 check.
