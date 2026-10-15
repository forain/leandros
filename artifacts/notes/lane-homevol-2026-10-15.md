# lane/homevol — persistent /home volume (2026-10-15)

User request: mount the second data disk at /home instead of /data, move the
tests from /data to /home, and stop recreating it on every build so user data
(Claude Code credentials, Firefox history, COSMIC settings) survives. Added
mid-lane: USB sticks and real hardware get a /home partition with all the space
left after the root.

Based on main d7f04ea2 (after the lane/laptop merge); rebased cleanly.

## Design

**One mechanism for every medium: an f2fs volume labelled `leandros-home`.**
`/etc/fstab` says

    LABEL=leandros-home  /home  f2fs  rw,nofail  0  2

and `mount(2)` resolves `LABEL=<f2fs volume_name>` (and `PARTUUID=`) itself
(`drivers/src/blkdev.rs` `resolve_source`; label on several volumes: the one on
the root's disk wins, then the boot disk, else ENOTUNIQ). QEMU's vdc image and
a stick's home partition carry the same label, so nothing hardcodes vdc.

**/dev/root auto-detection skips the home volume** (`resolve_root` step 3
ignores a volume labelled leandros-home). Checked with a `--no-cmdline` stick:
root -> sda2, /home -> sda4. NVMe paths from lane/laptop are untouched (NVMe is
neither removable nor `sd`, and only `f2fs_label_is` reads it).

**The image** (`scripts/mkfs-f2fs-home.py`): same on-disk layout as
mkfs-f2fs-populated.py, but the SIT grows with the volume (1 SIT block per 55
segments; a 2 TiB partition still fits) and SIT entries are indexed by main
segment, as the server reads them. 8 GiB sparse by default (costs ~14 MiB on
APFS when fresh). Seeded with the root's home skeleton: /home/leandro 0700
1000:1000, /home/cosmic-greeter{,/.config,/.cache,/.local} 0700 990:990.
Commands: create, ensure [--reset], head (metadata-only head for a disk
image), is-home, find-part (home partition on an MBR/GPT image or device),
clone (APFS clone / reflink / sparse copy).

**Kept across builds.** build-all.sh: `mkfs-f2fs-home.py ensure
f2fs-data1-<arch>.img` instead of `rm` + clone of data0. Creates it when
missing; an old data1 that is not a home volume (the old /data root clone) is
replaced ONCE with a printed message; `--reset-home` / `LEANDROS_RESET_HOME=1`
recreates it. run-qemu.sh does the same `ensure` (plus `--reset-home`,
`--scratch-home` = throwaway clone removed when QEMU exits). make-utm-vm.sh
keeps the VM bundle's own home copy across refreshes (`--reset-home`).

**New accounts.** init, after the fstab mounts, creates any missing
`/home/<user>` of /etc/passwd (0700, owner, plus .config/.cache/.local) and
never touches an existing one ("init: created /home/leandro" on the console).
No /etc/skel is shipped, so nothing is copied.

**Fallback.** No home volume (Pi SD card, an old stick, a stick too small):
the mount fails (`nofail`), init warns, /home is the root's own /home, which
keeps its seeded skeleton for this reason.

**USB stick / real disk** (`scripts/write-usb.sh`): layout ESP, root, home,
log (log stays MBR entry 3 / found by type; home is MBR entry 4 type 0x83, GPT
type 933AC7E1-... "LEANDROS home"). The script partitions the device itself
from its real size, so the home partition is sized at write time to "all the
rest" (no grow-on-first-boot needed). Only the home head (~14 MiB: SB, CP,
SIT, NAT, seeded dirs) is written. The root partition is reserved at
`LEANDROS_USB_ROOT_GIB` (16, capped at half the device, never below the root
image) so the home partition starts at the same place build after build.
Real sticks: PC 61 GB -> root 16 GiB, home 40.7 GiB; laptop 7.7 GB -> root
3.64 GiB (the image), home 3.0 GiB. `--image`: home `LEANDROS_USB_HOME_GIB`
(8) GiB, sparse (image file 25 GiB virtual, ~2.3 GiB on disk).

**Rewriting a stick keeps /home**: the target's partition table is read first
(`find-part`; a device needs sudo for the read), and a leandros-home volume at
exactly the new home partition's start and size is reused untouched. One
elsewhere stops the script ("pass --reset-home"); `--reset-home` writes a fresh
one. Plain `--image <out>` without `--sparse` replaces the file and its /home.
run-qemu.sh --usb-stick now regenerates an existing image in place with
`--sparse`, so its /home survives rebuilds. sparse-write.py: `--home OFF`
forces a fresh home's metadata; it also skips the source's holes (SEEK_DATA)
instead of scanning ~25 GiB of zeros (scan of the 25 GiB image: 1.7 s).
`--full` writes the root and the home head as two dd's instead of dd'ing the
root reservation's zeros.

**Test isolation.** driver.py `start` boots a fresh copy-on-write clone,
`f2fs-data1-<arch>.run-<RUN_ID>.img` (replaced at the next start, removed by
`stop`), never the persistent image, so automated runs neither dirty the
user's home nor share one writable image. `LEANDROS_PERSIST_HOME=1` boots the
persistent one. driver_nobreak.py uses `snapshot=on`.

**Tests.** vfstest, permtest, exectest: /data -> /home, each in its own dir
(/home/.vfstest, /home/.permtest, /home/.exectest-redir). /home is a separate
mount, so the cross-volume cases (absolute symlink back through the mount
point, tmpfs -> f2fs symlink) mean what they did. vfstest now removes the 15
files its xattr/ACL cases leave behind, then its dir. "/data" stays in the
kernel's mount-point interning table (harmless).

**Claude Code.** claude-run binds /home into /opt/alpine; checked with a fake
root (`CLAUDE_CODE_ROOT=/opt/fake alpine-run sh -c 'echo > /home/leandro/x'`):
the file lands on the mounted vdc volume, so ~/.claude persists. The Alpine
root itself (/opt/alpine, the CLI install) is on the root fs and is still
wiped by every build.

## Results (rebased tree, release, both arches)

| check | aarch64 (HVF) | x86_64 (TCG) |
|---|---|---|
| /home mounted from vdc by label | yes (`[BLK] LABEL=leandros-home -> vdc`) | yes |
| regress (20) + ptytest + privtest + permtest | 22/23: chroottest `chroot_unix_socket` flaky, see below | 23/23 |
| COSMIC login as leandro + Firefox (ffsession, persistent home) | ok | ok |
| file in ~, COSMIC setting (CosmicTheme.Mode is_dark=false), Firefox history (file:///tmp/fftest.html in places WAL) persisted across reboot | yes | yes |
| ... and across build-all.sh | yes (twice, incl. across the rebase rebuild) | yes |
| build-all.sh --reset-home | home empty-seeded again | (same code path) |
| init recreates a removed /home/leandro | yes | - |
| --usb-stick: root sda2, /home = home partition, persists across reboot | MBR, sda4: yes | GPT, sda5: yes |
| stick rewritten with write-usb.sh --sparse keeps /home | yes | yes |
| --no-cmdline stick: root auto-detect skips home | sda2 | - |
| stale root-clone data1 replaced once by build-all | yes | - |

Pre-rebase (on 9f4f1753) regress + ptytest + privtest + permtest were 23/23 on
both arches.

**chroottest `chroot_unix_socket` is flaky on main d7f04ea2 itself**: built
main unmodified (aarch64) and ran it 4 times: 3 FAIL ("host connect to the
jail's socket failed (errno 3)"), 1 pass. On this lane 2 of 3 failed. It
passed every time on 9f4f1753. Not caused by this lane (the case uses /var/tmp
and /tmp only); a regression from the lane/laptop merge or its vfs fd-table
port.

## What the user does once after merging

- Nothing for QEMU: the next `./scripts/build-all.sh` (or run-qemu.sh)
  replaces the old `f2fs-data1-<arch>.img` (a /data root clone; it held test
  junk only, the user's real /home lived on the root and was wiped by every
  build anyway) with a fresh home volume, printing a message. From then on it
  is kept.
- Sticks: the next write-usb.sh run onto an old stick finds no home volume and
  writes the new layout with a fresh /home; later rewrites keep it.
- The persistent image is the main checkout's `f2fs-data1-<arch>.img`; other
  worktrees have their own.

## Open items

- f2fs server has no segment cleaner: a segment is reused only once ALL its
  blocks are free. Long-lived churn (Firefox profile) on the persistent home
  can fragment the volume until it reports ENOSPC with free blocks left. 8 GiB
  delays this; a cleaner is the real fix.
- Pi 5 SD card (prepare-rpi5-sdcard.sh) has no home partition yet; /home stays
  on its root.
- /opt/alpine (Claude Code CLI) lives on the root and is wiped by builds; only
  ~/.claude persists.
- A stick written before this lane must be rewritten once (new layout, fresh
  /home). A root image that outgrows the 16 GiB reservation (or the laptop
  stick's tight one) needs --reset-home, after backing up /home.
- Old investigation scripts under artifacts/m6-session-data still log to /data.
