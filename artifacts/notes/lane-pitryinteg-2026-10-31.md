# lane/pitryinteg 2026-10-31 -- L1 (pitrykern) + L2 (pitryctl) integrated

Branch lane/pitryinteg = main b8c28dee + merge lane/pitrykern + merge lane/pitryctl + d40e8a59 (fixes). Not pushed.

## Interface check (L2 assumption -> result)
| Assumption | Result |
|---|---|
| BLKWRWIN 0x410812F0, 512 B units, <=16 ranges | verified (servers/vfs/src/block.rs: _IOW(0x12,0xF0) 264 B, WRWIN_MAX_RANGES 16, 512-byte units, root only, whole-disk node; vdc is a whole-disk node) |
| ranges relative to p1 | verified: vdc is MBR p1 bounded by length, sector 0 = partition start |
| 512 B pwrite = one program cycle | verified per L1 note (sector-granular read/compare/skip write); not exercised on hardware |
| HOLD = reboot(0x54424844, arg!=0) | verified (power.rs TRYBOOT_HOLD, lease 300 s); release = arg 0 (L2 never calls release: hold lapses by lease or the commit's CONFIRM disarms; fine) |
| CONFIRM = 0x54424F4B | verified |
| RESTART2 "0 tryboot" | verified: kernel matches whitespace token "tryboot" |
| BLKFLSBUF 0x1261 drops read cache | verified constant (block.rs flush_dev); effect on cache untested |
| deadline-left read interface | NOT added: no trivial proc hook; status shows nothing. Open |
No L1/L2 code change was needed for any of these.

## Fixes
- drivers/src/blkdev_sdhci.rs: duplicate set_rw_list stub (main + pitrykern both added) -> one copy.
- scripts/remote.py generic `status`: now prints cmdline length + sha8 + leandros.slot/kid tokens only. Other remote.py/pi_card.py prints audited: none left (remote.py:877 reads cmdline into a variable for kexec only, never printed). Not touched: scripts/pi-serial-push.py:310 and some *-test.sh scripts still `cat /proc/cmdline` (out of scope; audit item).

## Tests
- Host: pitest 4/4, pi_card_test.py 13/13.
- QEMU aarch64 (release, build-all --arch aarch64): boot + login ok; `reboot tryboot-hold/-release` ok; `/usr/sbin/espctl pi status` runs (on virt vdc is the f2fs /home, read-only open, reports "not a 512 B-sector FAT32 volume"); leandros.tryboot=1 tryboot.deadline=20 -> "[tryboot] deadline expired without confirm: plain reboot".

## Pi build
`./scripts/build-all.sh --rpi5` on a clean tree at d40e8a59: kernel-direct build tag d40e8a5940-20261031T1622Z (HEAD d40e8a59, no -dirty); CONFIG_BOARD_RPI5=y in target/kconfig-aarch64/.config. This note is committed after the build (docs only).
espctl (with `pi`) is installed as /usr/sbin/espctl in the ROOT image, not the initrd. pi-deploy swaps only kernel+initrd, so userland needs `remote.py pi-sync` (or equivalent root update) before `pi-card` verbs work on the Pi.

## pitryfix (d1b11fc5, 8b6ed13c)
- Bug 1: `find_boot_partition` read block 0 through the f2fs-offset device, so the 0x55AA check never saw the MBR and /dev/vdc never appeared. It now reads through a raw copy (offset 0, no limit) and logs `[sdhci] boot FAT (MBR p1) not exposed: <no 55AA signature | type 0xNN | unaligned lba N len M | MBR read failed>`.
- Bug 2: new read-only ioctl BLKISBOOTFAT (_IOR(0x12,0xF1,int) = 0x800412F1) in servers/vfs/src/block.rs returns the boot-FAT (`special`) flag. `espctl pi` verbs other than `status` (setup/stage/try/commit/abort/rollback, with or without --dry-run) refuse unless it returns 1. `status` prints the verdict first.
- Tests: pitest 5/5 (new guard_refuses_writes_off_bootfat); QEMU aarch64 release: `/usr/sbin/espctl pi setup --dry-run --dev /dev/vdc` -> "refusing, /dev/vdc is not the boot-FAT device".
- Pi build: `build-all.sh --rpi5` at 8b6ed13c, tag 8b6ed13c4a-20261031 (no -dirty), CONFIG_BOARD_RPI5=y, "exposed" string present. espctl: userland/espctl/target/aarch64-unknown-linux-musl/release/espctl.
