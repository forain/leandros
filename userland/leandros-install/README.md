# leandros-install

Privileged, headless installer backend (std/musl, installed as
`/usr/sbin/leandros-install`, run as root). The GUI runs it through `sudo`.

```
leandros-install list [--json] [--allow-removable]
leandros-install install --target /dev/X --yes [--root-gib N] [--allow-removable]
                         [--no-verify] [--zero-fill] [--json] [--dry-run]
leandros-install install --mode dualboot --target /dev/X --use-free --yes [--root-gib N]
                         [--json] [--dry-run]
leandros-install repair-esp [--boot DEV] [--esp DEV]
leandros-install mkhome --out FILE --size SIZE [--uuid HEX] [--time EPOCH] [--full]
```

`install` refuses (exit 1, no override) when the target is the boot disk
(the disk holding this boot's `root=PARTUUID=`), holds a mounted partition,
is too small (about 19 GiB), is read-only (non-NVMe), or is removable without
`--allow-removable`; and when the caller is not root (`--dry-run` excepted).
Without `--yes` it writes nothing.

Layout written (GPT, fresh random GUIDs): p1 ESP 512 MiB FAT32 "LEANDROS",
p2 root (max(16 GiB, f2fs size + 1 GiB), a block copy of the live root with
zero blocks skipped and a SHA-256 verify), p3 log 64 MiB at the end, p4 home
(the rest, fresh seeded `leandros-home`). Every `*.conf` on the ESP gets
`root=PARTUUID=<p2>`, loses `usbms.*`; NVMe targets gain `nvme.rw=1`. `/` is
remounted read-only during the root copy and restored on exit or signal.
NVMe writes: `install::enable_nvme_write` (BLKROSET 0; the kernel side comes
from the nvmerw lane, TODO there).

## Dual-boot, free space only (`--mode dualboot --use-free`)

Installs next to an existing OS (v1a). It adds partitions only in unallocated
space and never writes an existing partition except the files below on the
shared ESP. The disk must have a valid GPT (primary = backup, pure protective
MBR, 512-byte sectors), exactly one ESP in the systemd-boot layout
(`/loader/entries`), and a gap of at least 19 GiB (boot 512 MiB + root >= 16 GiB
+ home >= 2 GiB + log 64 MiB). Gate results are `gate` events (design B section
6: G1 stick, G2 disk, G5 size, G6 ESP, G7 backup space, G8 AC power; the Linux
filesystem gates G3/G4/G9-G12 report `na`); any `fail` aborts before any write.

Written, in order (every write goes through a `BLKWRWIN` window held open for
the whole run: the GPT areas and the gap, then only the ESP for the last step):

1. `backup` to the stick's `/home/dualboot-backup/bk-<epoch>/`: first and last
   MiB of the disk, `gpt.txt`, the full ESP image (`esp.img`), `RESTORE.txt`,
   `SHA256SUMS`; files are fsynced, then `sync()` (the stick may run
   `f2fs.fsync=lazy`, under which fsync returns early but `sync()` does not),
   then read back and compared.
2. `gpt`: four entries are added in the largest gap (disk GUID, ESP and Linux
   entries untouched): p<n> boot 512 MiB (type `0E02ED9A...`), root, home, log
   (64 MiB at the end of the gap), all with attribute bit 63 and 1 MiB aligned.
3. `boot`: the stick's ESP tree on a private FAT32 boot partition ("LEANDROSBT"),
   every `*.conf` rewritten to `root=PARTUUID=<root>` plus
   `blk.rw=PARTUUID=<root>,PARTUUID=<home>,PARTUUID=<log>,PARTUUID=<boot>` (not
   `nvme.rw=1`), and the `leandros/owner` marker.
4. `log`, `home` (fresh volume), `root` (block copy, zero blocks written where
   the target is not already zero), `verify`.
5. `esp`: `/EFI/leandros/BOOT<ARCH>.EFI` (Limine), `/EFI/leandros/limine.conf`
   (`protocol: efi`, `path: guid(<boot>):/EFI/BOOT/BOOT<ARCH>.EFI`),
   `/loader/entries/leandros.conf` (`sort-key zz-leandros`: Linux stays the
   default) and `/EFI/leandros/.leandros-owned`, read back from the medium.

`--dry-run` evaluates all gates and prints the `plan` (`old_gpt`, `new_gpt`,
`esp_files`, `cmdline_rw`, `backup_dir`) and the rewritten `limine.conf`; it
writes nothing. `list --json` disks gain `gaps` (`[{start, sectors}]`),
`dualboot` and `dualboot_reason`. Events: `gate` (`id`, `name`, `status`
`pass|fail|na`, `msg`); `plan` has `"mode":"dualboot"`; `step` names are
`backup`, `gpt`, `boot`, `log`, `home`, `root`, `verify`, `esp` in that order;
`done` also carries `boot_partuuid`, `backup_dir`. Test:
`scripts/dualboot-free-test.sh aarch64`.

## `--json`: one object per stdout line (the GUI contract)

All lines carry `"ev"`. Errors go to stdout as an `error` event in JSON mode
(and the exit status is 1); in text mode they go to stderr.

| ev | fields |
|---|---|
| `disk` (`list`) | `name`, `dev`, `size` (bytes), `model` (may be empty), `bus` (`nvme`/`virtio`/`sd`/`mmc`), `removable`, `ro`, `boot`, `eligible`, `reason` (empty when eligible; `boot disk`, `has mounted partitions`, `too small`, `read-only`, `removable`), `parts`: `[{n, kind, size}]` |
| `plan` | `target`, `bus`, `size`, `root_partuuid`, `*_start`/`*_sectors` for `esp`, `root`, `home`, `log` (512-byte sectors), `f2fs_bytes`, `nvme_rw` |
| `conf` (`--dry-run`) | `old`, `new` text of the boot ESP limine.conf |
| `step` | `name` (`gpt`, `esp`, `log`, `home`, `root`, `verify`), `pct` 0-100. Emitted in that order. While `root` runs, `/` is read-only. |
| `info` | `msg` |
| `done` | `target`, `root_partuuid`, `disk_guid`, `root_sha256` (`{"ev":"done","dry_run":true}` for a dry run) |
| `error` | `msg` |

Source layout: `plan.rs` (disks, eligibility, layout), `gpt.rs`, `esp.rs`
(FAT32 + conf rewrite), `copy.rs` (root block copy), `mkhome.rs` (home head),
`install.rs` (the flow). `dev.rs`, `disks.rs`, `conf.rs` are espctl's, included
by path. Acceptance test: `scripts/installer-test.sh aarch64 virtio`.

## Partition types

LeandrOS partitions use their own type GUIDs and carry GPT attribute bit 63
(no-auto) so Linux gpt-auto never mounts them. The legacy types
(0FC63DAF root, 933AC7E1 home, 042B7470 log) are still recognised by the
kernel (`drivers/src/partscan.rs`) and espctl, so older sticks keep booting.

| role | type GUID |
|------|-----------|
| root | `3F2D991B-D385-4FFF-9CF1-A0DB03062DE5` |
| home | `F936DEE1-99D9-4BBC-8D2A-B7447DB9F650` |
| log  | `3EC8FA02-816A-4D04-8718-64C3771C5187` |
| boot | `0E02ED9A-FEEE-4CAB-A22F-C861AD7DE307` |

Never use XBOOTLDR (`BC13C2FF`) or 933AC7E1 for LeandrOS partitions.

## GPT editor and `gaps`

`gpt::Gpt` reads an existing table (512 or 4096-byte sectors, entry geometry
from the header), and `shrink`, `add`, `set_attrs` change only the named
fields; every other entry byte and header field is kept. `write` writes the
backup entries+header first, then the primary, then re-reads and verifies both
CRCs. `leandros-install gaps --dev X [--sector-size N]` lists the entries and
unallocated 1 MiB-aligned ranges of at least 1 MiB. `mkhome` explicitly zeroes
all f2fs metadata blocks `[0, main_blkaddr)` before writing the head.
