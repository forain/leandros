# lane/blkroot — 2026-10-05

Branch `lane/blkroot`, base `7d114ce` (main). Not merged, not pushed.

Goal: the block-layer half of "boot from a USB stick whose f2fs root is on the stick". This lane adds a runtime multi-backend block registry, partition scanning, and `/dev/root` selection. xHCI, BOT/SCSI and the USB harness are separate lanes. An integration lane will register the USB disk here.

## Commits
- `15db43f` drivers: MBR/GPT partition scanner with host unit tests
- `cab4ee0` Runtime block registry, partitions, and /dev/root (drivers + vfs + kernel)
- `d14b12a` init: mount /dev/root with a wait for late disks; /data is nofail
- (this note)

## What changed
- **`drivers/src/blockdev.rs`** (always built) holds `trait BlockDevice`, `BlkErr`, `DiskDesc`, `RootErr` and `BootDisk`.
- **`drivers/src/blkdev.rs`** is `drivers::blkdev` on virt/x86_64. It is a registry of `&'static dyn BlockDevice` entries, 64 slots, append-only.
  - Whole disks are named `<class><letter>`, with a separate letter counter per class.
  - Partitions are further entries named `<disk>N`, or `<disk>pN` when the disk name ends in a digit. Each records its parent and start sector.
  - The old 4096-byte façade is kept with the same signatures: `init`, `device_count`, `read_block`, `read_blocks`, `write_block`, `flush`, `has_f2fs`, `info`. It converts blocks to sectors, adds the partition start, and refuses I/O past the partition end.
  - `has_f2fs` is false for a partition that does not start on a 4096-byte boundary.
  - `REG` is only held to copy an entry in or out. No global lock is ever held across I/O.
- **virtio-blk** (`virtio_blk::probe_all`): each disk is a leaked `VirtioDisk` with its **own** lock and 512-byte sector I/O. Before this, one global table lock was held across every request on every disk.
  - Probe order is unchanged. All virtio disks register first. Partition scanning runs only after that, so vda/vdb/vdc stay at indices 0/1/2.
  - The boot image's GPT ESP becomes `vda1` at index 3.
- **rpi5/raspi4b** keep the sdhci compile-time path. `blkdev_sdhci.rs` re-exports `sdhci`'s functions unchanged and adds the name and root helpers (vdX naming, root = index 1). `register()` returns `Unsupported`.
- **`drivers/src/partscan.rs`** is a pure parser over a read callback.
  - MBR: primary entries only. Superfloppy FAT boot sectors are rejected by the 0x00/0x80 boot-flag rule. Extended-partition containers are skipped.
  - GPT: found through a protective 0xEE entry. The header CRC, `my_lba` and entry-array CRC are all checked. If any check fails, the parser falls back to the backup header at the last LBA. If both headers are bad, the disk has no table.
  - Also provides PARTUUID text forms (GPT mixed-endian; MBR `ssssssss-pp`).
- **VFS** (`servers/vfs/src/block.rs`): `DiskOps.disk_bytes` was replaced by `DiskOps.describe(i) -> Option<DiskInfo>`, which returns name, parent, pno, start, bytes, lbs, removable, scsi and ordinal without doing any I/O.
  - Registry entries are mirrored lazily. Every name lookup or enumeration first syncs the registry if `disk_count()` has grown, so a disk registered at runtime shows up in `/dev` and `/sys/class/block` on the next lookup.
  - Kernel-scanned partitions become `Backing::Part` of their disk's slot. They therefore get proper `partition`/`start` sysfs attributes and appear as children of the disk. BLKPG and loop devices are unchanged.
  - Major/minor numbers: 254 for vd, 8 for sd; minor = ordinal×16 + pno.
  - sysfs `removable` now reports the device's flag.
- **Kernel**:
  - Two new Limine requests: `ExecutableCmdlineRequest` and `ExecutableFileRequest`. Their responses are copied in `kernel_main` before bootloader memory can be reclaimed. Base revision stays 6.
  - `sys_mount` accepts `/dev/<name>`, `/dev/root`, or a bare index.
  - `/dev/root` resolution, in order:
    1. `root=/dev/<name>` or `root=PARTUUID=…` on the cmdline. If an explicit `root=` matches nothing, the answer is ENODEV; the kernel does not guess.
    2. Index 1, if it is a whole `vd` disk with f2fs magic.
    3. The unique removable or `sd*` disk/partition with f2fs magic. Ties are broken first by the boot disk (Limine's GPT GUID or MBR signature), then by partition type 0x83 / Linux FS GUID.
  - Results: nothing found → `-ENODEV`. Ambiguous → `-ENOTUNIQ` (76), plus a log line listing the candidates. Success logs `[BLK] /dev/root -> <name>`.
  - New `SYS_BLKDEV_NAME` (523). `lsblk` uses it, so it prints `/dev/vda1` instead of the bogus `/dev/vdd` it would otherwise derive from index 3.
  - servers/f2fs takes its `/proc/mounts` device names from the registry.
- **init**:
  - Mounts `/dev/root`. On ENODEV it retries every 100 ms for up to 30 s, printing once per second.
  - Falls back to `/dev/vdb` on EINVAL (a kernel that does not know `/dev/root`) or on timeout.
  - fstab root line is now `/dev/root`, and `/dev/vdc /data` has `rw,nofail`. The fstab walk already only warned on failure; for `nofail` entries the warning now says so.

## Integration API (USB lane)
```rust
use drivers::blkdev::{self, BlockDevice, BlkErr};
// implement on your USB MSC LUN (sector_size 512 or 4096; own internal lock;
// name_class() -> "sd"; removable() -> true)
pub fn register(dev: &'static dyn BlockDevice) -> Result<usize, BlkErr>; // Box::leak your device
```
- `register` does I/O: sector 0, the GPT headers and the entry array. Call it from thread context after `blkdev::init()`.
- It returns the disk's index. Partitions get the next indices, named `sda1`, `sda2`, and so on.
- Read/write buffers are always whole sectors and can be arbitrarily long. The device splits them into its own transfer size.
- `capacity_sectors()` may be 0 if unknown. In that case there is no backup-GPT fallback and no end-of-disk bounds check.
- Nothing else is needed: VFS nodes appear on the next lookup, and `/dev/root` picks the stick up on init's next retry.

## Evidence
- Host tests: `cd drivers/partscan-tests && cargo test` → 14 passed. They cover MBR, GPT, backup GPT (corrupt header, and corrupt entry array), both headers bad, misaligned partition, superfloppy, 4Kn disk, wrong `my_lba`, and PARTUUID text.
- `cargo check -p kernel` passes for aarch64, x86_64, aarch64+rpi5 and aarch64+raspi4b.
- `build-all.sh` was run for both arches: BUILD_RC=0. They were built separately because the disk filled up during the combined build; see Open.
- **aarch64 (HVF)**, default boot. Serial excerpt; the boot is otherwise the same as the main baseline log (`/tmp/leandros-ytfreeze-serial-full.log`) apart from the init wording:
  ```
  [BLK] /dev/vda: 1048576 x 512-byte sectors
  [BLK] /dev/vdb: 7127040 x 512-byte sectors
  [BLK] /dev/vdc: 7127040 x 512-byte sectors
  [BLK] /dev/vda1: start 2048, 1046495 sectors
  Mounting F2FS root /dev/root to /mnt...
  [BLK] /dev/root -> vdb
  [F2FS] /dev/vdb: checkpoint ver 1 is a clean unmount
  pivot_root successful! Root is now F2FS.
  Mounting /dev/vdc at /data (f2fs)...
  ```
  - `runtests regress`: 15/15 PASS.
  - disktester: 3× Format success, ending with `To mount: "/dev/loop0p3" as Root`.
  - ffsession `--nofirefox`: greeter, desktop and cosmic-term all reached.
  - `lsblk` lists vda/vdb(/)/vdc(/data)/vda1.
  - `/sys/class/block/vda1/{start,size,partition,dev}` read `2048 / 1046495 / 1 / 254:1`. `/dev/vda1` reads the FAT boot sector.
  - `mount /dev/vda1` (FAT) fails, and so does `/dev/sdq`.
- **aarch64 with a cmdline** (limine.conf patched into the image with mtools, then restored):
  - `root=/dev/vdb` → `[BLK] kernel cmdline: root=/dev/vdb`, then `/dev/root -> vdb`.
  - `root=PARTUUID=deadbeef-02` → `Waiting for the root device (0 s)...` through `(29 s)`, then `mount /dev/root failed (errno 19), falling back to /dev/vdb`, then normal boot to login.
- **x86_64 (TCG)**: same `[BLK]` lines (vdb/vdc are 7200768 sectors) and `[BLK] /dev/root -> vdb`.
  - `runtests regress`: 15/15 PASS.
  - disktester: Format success.
  - `lsblk` and `/dev/vda1` as on aarch64.
  - ffsession `--nofirefox`: greeter 1.2 s, desktop 14.6 s, terminal 1.3 s.

## Open / risks
- **Step (c) of `/dev/root` is not exercised.** QEMU has no `sd` device until the USB lanes land. It is exercised only by review. The integration lane should test:
  - a stick with GPT (ESP + Linux FS partition);
  - a raw f2fs stick;
  - two f2fs sticks, which should give ENOTUNIQ.
- **USB boot with QEMU's vdb also attached:** step (b) wins over the stick. To boot the stick, the harness must pass `root=` or omit vdb. The USB stick image should carry `cmdline: root=PARTUUID=…` in its limine.conf.
- **The ExecutableFile boot-disk tie-break was not observed live.** The GUID byte order is assumed to match Limine's on-disk copy.
- **`register` can interleave.** Two concurrent `register` calls can interleave partition indices. Names and parents stay correct.
- **No unplug.** Entries are permanent, and a pulled stick keeps its index; there is no removal path.
- **Full registry.** With all 64 entries used, registration fails with `Full`.
- `DiskOps.describe` replaced `disk_bytes`. This is an ABI change inside the kernel only; there are no other users.
- **Disk space.** The Mac data volume hit ENOSPC during a combined two-arch build (another lane was building too). Each worktree holds about 14 GB of f2fs images.
- **Symlink outside the worktree.** I created `.claude/worktrees/disks-rs → ~/code/disks-rs` so `build-all` builds disktester. It is outside this worktree.
