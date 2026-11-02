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

## pitrygeom: misaligned FAT geometry (real card: data start sector 1982, mod 8 = 6)
- The only alignment assumptions were `Fat::check_align` (refused cluster < 4 KiB or data0 % 8 != 0; used by setup and stage) and `Vdc::write` in picard_cli.rs (bulk 4 KiB pwrite when offset/len were 4 KiB multiples). Everything else was already sector-exact: cluster->LBA is data0 + (c-2)*spc, BLKWRWIN ranges come from merged exact LBAs, slot tails are zero-filled per file sector, FAT reads are 512 B sectors. pi_card.py has no alignment logic.
- Now: check_align only refuses cluster < 512 B or spc > 128 (FAT12/16 and sector size != 512 are still refused in open()); Vdc::write is strictly one 512 B pwrite per sector. status prints data0%8 instead of aligned=.
- Window count: Fat::allow still refuses a write set needing more than 16 merged ranges, before any write ("write set needs N windows (max 16)"). Refused rather than split: setup/stage allocate slot files contiguously, so the sets are small (setup/stage a handful of ranges; commit/rollback single sectors), and a split would add ordering states that the replay does not cover.
- Tests (pitest, 6/6): new power_loss_every_sector_misaligned runs the whole replay (setup, commit-a, stage, commit, shrink-stage, rollback, garbage variants, fsck.fat -n per phase) on an image with 512 B sectors, 8 spc, 32 reserved, 2 x 975 FAT sectors, data0 = 1982, 1,000,000 sectors. mkfs.fat always makes the FAT 976, so the empty mkfs image is rebuilt with 975-sector FATs (exact BPB values); only the first 3000 clusters are kept in memory, fsck pads to the BPB total. Aligned replay unchanged and green.
- Rebuilt userland/espctl/target/aarch64-unknown-linux-musl/release/espctl via build.sh. No QEMU smoke (kernel unchanged).

## pitrye2e: review must-fixes + real kernel-path e2e
- M1 servers/vfs/src/block.rs blkwrwin: the implied-RW (set_rw) branch is skipped when the disk is `special` (boot FAT); write_chunk already lets WinCheck::Inside through. M2 drivers/src/sdhci.rs do_io: a failed compare read returns false, never writes. setup: if the new root entries fill their sector, the next root sector must start with 0x00 (or the cluster ends there: refused).
- Torn variants added to the setup replay on every sector except the root sector. Running setup without accept_root_risk is refused by the gate (asserted); a garbage root sector loses config.txt by construction (that is what --accept-root-sector-risk accepts), so it is not replayed torn. pitest 6/6.
- Kernel knob `sdhci.test_bootfat=<virtio ordinal>` (drivers/src/blkdev.rs, parsed in set_rw_list; inert unless given; not present on rpi5 where blkdev_sdhci replaces blkdev): that whole virtio disk reports read-only + special (hidden, BLKISBOOTFAT=1). Caveat: the virtio path writes 4 KiB RMW blocks (identical bytes in the 7 other sectors), unlike sdhci's 512 B compare-skip.
- scripts/pi-e2e-test.sh (aarch64; after build-all --arch aarch64 and espctl/build.sh aarch64; hold the qemu lock): extra virtio disk = exact card geometry image (pitest `emit_e2e_image`, env PICARD_E2E_IMG). Result 2026-10-31: `pi setup` really ran (891 sector writes), `pi status` ok, fsck.fat -n clean, 779 sectors changed: 33-321, 1008-1296 (FATs), 1982 (root), 2990-3294 (new dirs/files); nothing in 0-31 or any existing file's clusters; config.txt, kernel.img, cmdline.txt, initrd.cpio, dtb byte-identical; slots/ present. `pi stage` NOT run: it requires the real good initrd (/boot/pi/good/initrd sha gate) which a QEMU guest lacks.
- Pi build: `build-all.sh --rpi5` at 04f966b8, tag 04f966b8f0-20261031 (no -dirty), CONFIG_BOARD_RPI5=y. aarch64 espctl rebuilt (userland/espctl/target/aarch64-unknown-linux-musl/release/espctl).

## Real Pi 5 card, 2026-10-31/11-01 (orchestrator)
- `pi setup` on the real card (user-approved, no --accept-root-sector-risk): 1179 sector writes, root-risk=false; cold power cycle afterwards booted the old card kernel normally.
- Good promoted to ee9b3a680385 (lane/rpi5fix ee9b3a68 = main edc874b0 + Pi build fix), after checking uname, trial sha 16c6633579e284d2 and no =m. New espctl at /usr/local/sbin/espctl on the Pi root (the image's /usr/sbin copy is older).
- Migration: `pi-card try` booted the factory kernel through the migrated tryboot.txt (TRY OK); `pi-card commit` wrote the `[all]` copy into config.txt sector 3.
- `pi-card stage` -> s0 (k8=cf29b898, i8=d815db67, read back); `pi-card try` -> firmware booted ee9b3a6803 straight from slots/s0 (uptime ~30 s, no kexec, deadline armed, eth0 + wifi0, root f2fs); `pi-card commit` -> config.txt sector 0 `os_prefix=slots/s0/`, "[tryboot] confirmed: deadline disarmed".
- Cold power cycle 2026-11-01: firmware booted ee9b3a6803 from config.txt alone (uptime 24 s at first SSH, no kexec/tryboot lines, root /dev/vdb f2fs, eth0 192.168.2.12 answers ping, Wi-Fi SSH). `espctl pi status`: config.txt sha8=3728c720, s0 state=good, cmdline copies match. Factory kernel.img untouched.
- Box regress main 7dfc27d2 (includes this lane, rpi5fix and f2fsnid): 66 suites, 0 bad, both arches.

## /proc/tryboot on the real card, 2026-11-02

Main b9884f06 (`b9884f062b-20261102T0543Z`): pi-deploy (kexec, `/proc/tryboot`
`state=off`) -> pi-promote -> `pi-card stage` -> s1 -> `pi-card try`: firmware
booted slots/s1 at uptime 22 s, `/proc/tryboot` `state=armed deadline_s=900
left_s=878 holds=0` -> `pi-card commit`: config.txt `os_prefix=slots/s1/`,
`state=confirmed left_s=0 holds=1`, klog "[tryboot] hold: commit in progress",
"[tryboot] confirmed: deadline disarmed". s0 (ee9b3a6803) is now `prev`.
The root's espctl predates 4889e2ec, so `espctl pi status` has no `tryboot:`
line until a pi-sync. Card stage needs the default BUILD_TAG format
(`<rev>-YYYYMMDDTHHMMZ`); a custom LEANDROS_KERNEL_TAG is refused.
