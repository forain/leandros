# lane/pitryctl 2026-10-31 -- espctl pi + remote.py pi-card

Plan: orch/designs/plan-pitryboot-2026-10-31.md + review (M2, M3, M6, M8, M9 here; M1/M4/M5/M7 are kernel, M4 client half is here).

## What
- `userland/espctl/src/picard.rs`: raw FAT32 locator (LFN-aware), allowlisted sector writer, setup/stage/try/commit/abort/rollback/status. `picard_cli.rs`: /dev/vdc, BLKWRWIN windows, reboot(2), /proc, `espctl pi ...`.
- `scripts/pi_card.py` + `remote.py pi-card {status,setup,stage,try,commit,abort,rollback}`; every verb takes `--dry-run`.
- `pi-status` no longer cats /proc/cmdline (length + sha8 + marker). Generic `remote.py status` (line ~694) still cats it: stick/laptop path, NOT changed here, audit item.

## Design as built
- Layout: one root entry `slots/` (s0..s2: kernel.img 32 MiB, initrd.cpio 16 MiB, cmdline.txt, cmdline_try.txt, bcm2712-rpi-5-b.dtb, 512 B info), root `tryboot.txt`, root `cmdline_try.txt` (marker `leandros.tryboot=noprefix`; slot copies carry `=1`). No overlay copies.
- setup (M3): only entirely-free FAT sectors (base rounded up to 128 clusters, FAT1+FAT2 sectors verified zero); writes small data, then FAT chains (FAT2 then FAT1), then dirs (slot dirs, then slots/), then ONE root sector with 5 entries (slots, tryboot.txt, LFN cmdline_try.txt). Kernel/initrd clusters are not written by setup. Refuses if that root sector holds config.txt/kernel.img/initrd.cpio/cmdline.txt/*.dtb/overlays unless `--accept-root-sector-risk` (the real card's root is probably like that: E0 `pi-card setup --dry-run` reports it). FSInfo and dirty bit untouched.
- Migration (M2): (a) one comments-only sector S of config.txt becomes `[all]` + every non-comment line of sector 0's head (same length; refuses on [sections], duplicate keys, no suitable sector); tryboot.txt at setup = that A text + `cmdline=cmdline_try.txt`, so `try` boots the factory kernel with the flag (E1) and `commit` copies exactly sector S. (b) sector 0 becomes `os_prefix=slots/sN/` + comments.
- `commit` is generic: config.txt and tryboot.txt must differ in exactly ONE 512 B sector (over len(config)); that sector is programmed alone (one write call, one window), read back fresh, then CONFIRM. Slot commit also needs marker `=1`, running `uname -v` containing the staged tag, and slot sha re-verified from the medium. HOLD is sent first (M4); `--skip-hold` exists for kernels without it.
- stage (M6): info `writing` -> kernel+zero tail, initrd+zero tail -> fresh read-back sha + zero-tail check -> info `staged` -> one sector of tryboot.txt (the digit). Refuses: initrd sha != /boot/pi/good/initrd, good/prev slot, tryboot/config differing beyond sector 0 (pending migration), size overflow.
- M8: marker only via `/proc/cmdline` word match inside espctl; slot cmdline copies are compared byte-for-byte silently (message never includes content); status prints match/MISMATCH only. Tests assert the PSK never appears in any output.
- remote gates (stage): pi_check_elf, bin == llvm-objcopy(ELF), bid_check, no lib/modules in initrd and no `=m` and `CONFIG_BOARD_RPI5=y` in target/kconfig-aarch64/.config, BUILD_TAG regex from ELF != good tag, ELF sha == /boot/pi/good/kernel (`--unverified` overrides), initrd sha == good initrd, sizes. `try` waits wired 192.168.2.6 then Wi-Fi 172.16.162.184 and prints TRY OK / TRY DID NOT TAKE / NOTHING ANSWERS.

## Tests
- Host: `cd userland/espctl/pitest && CARGO_BUILD_TARGET=aarch64-apple-darwin cargo +stable test --release --offline` (pulls picard.rs via #[path]; needs mkfs.fat, mcopy, fsck.fat from brew; the pinned nightly has no host std). 4 tests: migration layout; whole-life power-loss replay (setup, commit-a, stage s0, commit-b, stage s1, commit, shrink-stage, rollback) checking after EVERY sector write that the firmware model (config.txt, [all]/section filters, os_prefix viability, cmdline) boots the factory kernel or a complete slot with the exact root cmdline, plus 3 garbage variants (one with a nasty `[pi4]`/NUL header) for every config.txt sector program; fsck.fat -n after each phase; gates; dry-runs leave the image byte-identical; refused setup writes nothing. A deliberate mutation (drop the [all] copy) fails the replay.
- Python: `python3 scripts/pi_card_test.py` (13: =m initrd, =m kconfig, non-rpi5, bin != objcopy, sha != good, initrd != good, tag == good, oversize, redaction, every verb --dry-run issues no write).
- `cargo check --target aarch64-unknown-linux-musl` of espctl passes. NOT run: QEMU virt e2e on a second FAT disk, real build via build.sh (zig).

## Assumptions about L1 (read from ../leandros-pitrykern, may change)
- `/dev/vdc` = partition node, sector 0 = partition start; `BLKWRWIN` (0x410812F0) start/len are in 512 B units relative to that node, up to 16 ranges (setup uses ~5, stage ~6).
- A 512-byte `pwrite` on vdc is one program cycle; espctl issues commit/info/tryboot writes as exactly 512 B writes and bulk data only 4 KiB-aligned.
- `reboot(.., 0x54424844, arg)` HOLD (arg != 0 hold), `0x54424F4B` CONFIRM, RESTART2 "0 tryboot" (from kernel/src/power.rs).
- `BLKFLSBUF` (0x1261) on the read fd drops the cache; otherwise read-back could be served from a VFS cache (best effort, then the file is reopened).
- Deadline-left is not shown in status (no read interface known).

## Open
- Real config.txt may not be migratable (needs a comments-only sector bounded by comment lines, unique keys, no sections); E0 dry-run tells.
- abort = plain RESTART (kernel clears the flag, M5). No QEMU e2e yet.
