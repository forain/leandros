# leandros-install

Privileged, headless installer backend (std/musl, installed as
`/usr/sbin/leandros-install`, run as root). The GUI runs it through `sudo`.

```
leandros-install list [--json] [--allow-removable]
leandros-install install --target /dev/X --yes [--root-gib N] [--allow-removable]
                         [--no-verify] [--zero-fill] [--json] [--dry-run]
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
