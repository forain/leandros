# Filesystems and storage

LeandrOS stores its root and data volumes on a real, read-write **F2FS** implementation ([`servers/f2fs`](../../servers/f2fs)), keeps extended attributes and POSIX ACLs in one shared contract crate ([`servers/xattr`](../../servers/xattr)) that every filesystem and the kernel interpret identically, and exposes disks to userspace through a Linux-shaped block layer — `/dev/vd*`, `/dev/sd*`, partitions, loop devices, BLKPG and a synthesised `/sys/class/block` — good enough to run the unmodified [disks-rs](../userland/ports.md) partitioning and formatting stack.

## Overview

| Filesystem | Mounted at | Backing | Served by |
|---|---|---|---|
| `f2fs` | `/`, `/data` | a block device (virtio-blk, USB mass storage, or SD on the Raspberry Pi) — real persistent storage | `servers/f2fs`, one instance per volume |
| `tmpfs` | `/tmp`, `/dev/shm`, `/run/user` (and `/run/user/<uid>`) | anonymous memory, page-shared across processes | `servers/vfs` ([vfs.md](vfs.md#tmpfs)) |
| `procfs` | `/proc` | synthesised | `servers/vfs` ([vfs.md](vfs.md#proc)) |
| devices | `/dev/*` | server-backed nodes | `servers/vfs` and the device servers |
| sysfs (partial) | `/sys/class/block` synthesised; the rest of `/sys` staged on disk | — | `servers/vfs/src/block.rs` |

In QEMU the boot disk is `vda` (the Limine image with its ESP), the root volume `vdb` and the data volume `vdc`; `init` mounts `/dev/root` (see [/dev/root](#devroot-and-booting-from-usb)) on `/` with `pivot_root`, then `/dev/vdc` on `/data` (`rw,nofail`).

## F2FS

### What is implemented

The f2fs server is a real implementation, not a stub:

- **Layout.** Direct, indirect and double-indirect block pointers (no triple-indirect level, so the maximum file is about 3.95 TiB); multi-block directories with regular 4 KiB dentry blocks; hardlinks and symlinks. Images are built with `-O ^extra_attr,^inline_data,^inline_dentry`, so every inode uses standard block pointers — the image builder ([`scripts/mkfs-f2fs-populated.py`](../../scripts/mkfs-f2fs-populated.py)) and the server agree field by field on every on-disk constant.
- **Metadata.** Ownership and mode persist and `chmod`/`chown` are enforced; `atime`/`mtime`/`ctime` with nanoseconds live in inode bytes 36..72, previously unused (old inodes read back 0, which is what `stat` reported before); `st_blocks` comes from a real `i_blocks` at inode offset 28, gated by an `i_advise` bit (`0x80`) so no image format change was needed; xattrs live in a dedicated node block named by `i_xattr_nid` at inode offset 24.
- **Space.** Blocks are reclaimed on `unlink`, `rmdir` and truncate — including a shrinking truncate to a non-zero length (`truncate_to`, a dnode-tree walk). `statfs` is O(1): a valid-block counter is seeded from one SIT scan at mount and adjusted by `sit_mark_block_used`/`sit_mark_block_free`, the only writers of a `vblocks` field (it used to sum every segment's SIT count with the filesystem lock held, 5+ s on a USB stick; `f2fs.statfs_check=1` re-runs the full scan on every call and logs a mismatch). Recycled data blocks are zeroed before reuse.
- **Namespace.** POSIX `rename` atomically replaces its destination; `rename`, `unlink` and `rmdir` honour the sticky bit; `link(2)` to another user's file follows `protected_hardlinks`.
- **Durability.** `fsync` and `sync` really flush to the device, per file and with group commit (see [Performance on slow media](#performance-on-slow-media)). Namespace mutations checkpoint synchronously. Unmount and `remount,ro` write a clean-unmount checkpoint, and the next mount reports whether the previous shutdown was clean.
- **Other.** `fallocate` mode 0 grows `i_size` (f2fs allocates at write time), `KEEP_SIZE` is a no-op, punching holes is `EOPNOTSUPP`; read-only mounts refuse every mutation; `relatime` atime updates cost one cache lookup when the rule says no update.

### How it works

Each `mount()` spawns an independent server instance with its own IPC port, registered with the VFS at its mount point. The server is single-threaded and dispatch is synchronous ([README.md](README.md#a-server-is-a-port-with-a-direct-handler)), so its in-RAM state is consistent between requests. Per mount it keeps:

- an **indexed block cache** sized at 1/32 of RAM within 4 to 64 MiB (`f2fs.cache_kib=N` overrides), allocated lazily in 1 MiB chunks, with a hash index (O(1) find) and an intrusive LRU list that holds exactly the evictable slots (clean, unpinned), so eviction is O(1) and never writes;
- a 256-entry open-file table, each entry remembering the absolute path it was opened by (for `/proc/self/fd/N` and dirfd resolution);
- the checkpoint, NAT and SIT state in memory.

Regular-file data does not go through the block cache on the read side: it lives in the unified [page cache](../kernel/memory.md#the-unified-page-cache), which `read(2)`, `pread` and every mapping fault share. The open file's offset lives in `SLOT_FAST` behind a per-slot sleeping lock (`f_pos_lock`), so a read that hits the page cache needs no filesystem lock.

The path walk (`resolve_path_ex`) runs in volume-relative coordinates, checks search permission on each directory before looking into it, follows symlinks (relative targets against the link's own directory, absolute targets re-rooted at the jail root for a chrooted caller, at most 40 hops before `ELOOP`), and preserves the walk's errno (`resolve_path_r`) instead of collapsing everything to `ENOENT`.

### Design decisions

**Checkpoint at the end of every namespace mutation.** A runtime `mkdir` once produced `?---------` directories after a reboot. The on-disk formats were proven to agree; the defect was crash consistency. Mutations were made durable only by `fsync` or every 16 writes (`maybe_flush`), and the development harness hard-kills QEMU without unmounting — so LRU write-back would evict *some* of a `mkdir`'s blocks (the parent dentry) while the matching NAT and inode stayed in RAM and were lost, leaving a type-0 inode and a rewound `next_free_nid` that collided with later allocations. The fix: `flush_checkpoint` at the end of every namespace mutator, plus a device flush barrier between writing the cache and writing the checkpoint block (the commit record) and another after it. A hard-kill test validated the result. Several older workarounds that pre-created directories in the image date from this bug.

**A checkpoint is still the only commit, but `fsync` no longer always pays for one.** There is no log-structured roll-forward (fsync data is made recoverable only by a checkpoint), so the checkpoint is what makes any of it recoverable. `VFS_FSYNC` now carries the file and the kind of request, and every change records the first checkpoint that will contain it, per inode or globally for namespace changes; an `fsync` whose file has nothing uncommitted returns at once. Concurrent `fsync`s share one checkpoint. Heavier than Linux, but honest — the first `fsync` returned 0 unconditionally.

**The cache is sized for path walks, not for data.** With 4 slots, a single path component's working set (the directory's NAT and inode blocks, then per dentry block the NAT, inode and data block) missed on every lookup; a `stat` of a missing path cost ~2.8 ms of synchronous virtio-blk reads on KVM. `cosmic-greeter` stats ~750 missing icon-theme paths per repaint, so each keystroke waited 5–44 s. 192 slots fixed it (p50 35.5 s → under 200 ms, together with GPU rendering by default). `MountState` lives in a heap `Box` built in place, because building it on the then-64 KiB kernel stack once overran the stack and corrupted the allocation beneath it.

**Never hand out a block unzeroed, and never re-hand the current segment.** `alloc_data_block` once returned recycled blocks with their old contents — a cross-file information leak invisible on fresh images (reproduced by poisoning the free area with `0xAB`). And once block freeing existed, `sit_find_free_seg` had to exclude the current data and node segments, or a half-written current segment could reach zero valid blocks and be handed to the other log, overwriting live data.

**Verify by checksum, not by `df`.** During the reclaim work `df` numbers were deeply misleading (a stale free-segment count persisted in a dirty image across reboots). The diagnostic that separated "accounting wrong" from "blocks corrupted" was checksumming a large pre-existing file (`libc.a`) before and after create/delete churn; 30–40 cycles of 5 MB files, several times the free space, left it byte-identical across reboot. `statfs` now sums per-segment SIT valid-block counts.

**The VFS opcodes are duplicated in f2fs, deliberately and dangerously.** f2fs redeclares the `VFS_*` constants it dispatches on. An *undefined* upper-case name in a Rust `match` arm is not an error — it is a catch-all binding. When `VFS_LCHMOD`/`VFS_LCHOWN` arms were added without their constants, the first one swallowed every later arm and made `chown` a silent no-op on f2fs. **Any opcode added to f2fs's dispatch must also be added to its constant block.**

### Performance on slow media

The real-PC stick is a USB 2.0 drive whose small write commands take 0.5 to 1.3 s each and whose reads take a millisecond, and for a month every symptom on it ("the login hangs", "video stutters", "Firefox freezes") was the same defect: a global lock held across device I/O. The fixes are layered; each is a lane whose note under `artifacts/notes/` has the numbers.

| Change | What it fixed |
|---|---|
| `F2FS_MOUNTS` is a sleeping, fair FIFO/ticket lock (`sched::iolock::IoMutex`) with hold and wait accounting | Waiters used to spin with preemption off, burning three of four CPUs while one task waited on the stick (`[WDOG]` stalls) |
| Bulk file data is read with the lock dropped, validated by per-inode data generations | Reads no longer queue behind each other |
| 64 MiB indexed block cache; dirty pressure wakes a background write-back task instead of running a locked "emergency" checkpoint | 11 of 14 `[HUNG]` holders in the Firefox start were that checkpoint |
| Write-back every 30 s (once the oldest uncommitted change is that old), runs of up to 240 blocks (960 KiB, eight whole USB commands), phase 2 in 120 KiB requests, NAT blocks left clean when the entry is unchanged | 142 checkpoints and 113 fsyncs in 404 s of video playback was the stick's whole write budget |
| **No device I/O with `F2FS_MOUNTS` held** (`with_mount`) | Below |
| `statfs` in O(1) | `statfs` held the lock 5+ s |
| Per-file fsync, delayed group commit, optional lazy fsync, batched miss fetches | Below |

**`with_mount`: requests that cannot stall the filesystem.** Every request runs with the lock held and the block cache in no-I/O mode. A block that is not cached is not read: it becomes the request's *miss* and the request is *doomed* (missing blocks read as zeros, changes go to a scratch block, open-file slots are left alone, directory loops stop). Changes made before the miss are undone (cache slots from an undo log, open-file slots via `of_log`, allocator state and counters from a snapshot), the lock is dropped, the block is read into the cache, and the request runs again. Only the final, miss-free run takes effect, all under one hold, so requests stay atomic. The unlocked read is discarded if the block entered the cache meanwhile (per-block install generations), concurrent misses on one block share one read, and the reader is protected from a group-kill reap while others wait on it (`ksync::must_finish_begin/end`). A doomed run records up to 16 distinct misses and the next pass reads them in one unlocked sweep, nearby ones merged into single commands. Still done under the lock: mount (the SIT scan), unmount, remount-ro and shutdown, contexts that may not sleep, requests needing more than 1024 reads, and out-of-memory fallbacks; `[fslock]` counts them. Measured on an emulated slow stick (`usbms.wdelay_us=500000`), a cached `stat` of `/` next to a cold walk and two fsync writers: aarch64 HVF max 1.51 s to 277 us, x86_64 TCG max 1.99 s to 22 ms, and `[LOCKHOLD]` reports 60 to 0.

**fsync.** Three layers, all decided at the first mount from the kernel command line:

1. *Per file.* An fsync whose file has nothing uncommitted skips the checkpoint. Dirty [page-cache](../kernel/memory.md#the-unified-page-cache) pages are written to f2fs *before* that decision, so a file that exists only as a dirty `MAP_SHARED` page still counts as changed (`pagecache-crashtest.py` pins this).
2. *Delayed group commit* (default on). An fsync that needs a new checkpoint waits until `min(f2fs.fsync_delay_ms, 2 x the recent checkpoint time)` after the later of the window's first fsync and the previous checkpoint's end, so fsyncs arriving meanwhile share it. About 1 s on the stick, a few ms on virtio, so a fast disk is not slowed. The guarantee is unchanged: fsync returns only after a checkpoint that started after it arrived has completed; `EIO` if the checkpoint's writes fail.
3. *Lazy fsync* (`f2fs.fsync=lazy`, default off; **the NVIDIA test stick turns it on**). `fsync`/`fdatasync` return at once and a checkpoint starts within `f2fs.lazy_s` seconds. A power cut or reset in that window loses everything since the previous checkpoint, fsynced or not; the volume itself falls back whole to the previous checkpoint, as after any unclean shutdown. `sync`, `syncfs`, unmount, remount-ro and poweroff stay synchronous. The data-loss window is logged at mount. It exists because Firefox fsyncs took 4 to 12 s each on the stick (each forced a multi-second checkpoint).

Diagnostics every 5 s: `[fsync]` (top callers: pid, comm, path or inode, count, latency, delay, `CP_MUTEX` wait) and `[ckpt]` (checkpoints by reason; blocks, commands and time per region: data, node, NAT, SIT, SSA, CP, flush). A hold of `F2FS_MOUNTS` that was long is followed by a line naming the request and splitting its time by kind (read-miss, data-read, write, flush) and the part spent waiting for the USB disk lock.

**Exiting with a file open.** `exit_group` once reaped off-CPU siblings in place, so a thread parked in filesystem I/O left the (now sleeping) lock owned by a dead pid; and close-all/close-on-exec did not close f2fs files, so every exit with an open file leaked one of the mount's 256 slots. Both are fixed (see [kernel/invariants.md](../kernel/invariants.md) and `ksync`'s sleeping-lock count: a group kill defers a member that holds or waits for one until it returns to user space).

### Kernel command-line knobs

All are read once, from the kernel command line (`/proc/cmdline`, the stick's `limine.conf`); a key given twice takes the last value. `LEANDROS_USB_EXTRA_CMDLINE` and `nvidia-test-stick.sh --cmdline` append to the stick's.

| Knob | Default | Meaning |
|---|---|---|
| `f2fs.fsync=lazy` | strict | fsync/fdatasync return at once; a checkpoint starts within `f2fs.lazy_s`. Any other value (`strict`) is the default |
| `f2fs.lazy_s=N` | 5 (1..600) | the lazy-fsync checkpoint deadline |
| `f2fs.fsync_delay_ms=N` | 1000 (max 10000, 0 = off) | cap of the delayed group-commit window |
| `f2fs.fsyncbatch=0` | on | turns off per-file fsync, delayed group commit, lazy mode and batched miss fetches (a doomed run fetches only its first miss), for an A/B boot |
| `f2fs.nostall=0` | on | run requests the pre-nostall way: metadata misses read with the lock held, one run, no undo |
| `f2fs.cache_kib=N` | RAM/32, 4..64 MiB | block cache size (the probes shrink it to force misses) |
| `f2fs.statfs_check=1` | off | re-run the full SIT scan on every `statfs` and log mismatches |
| `pcache=0` | on | no page cache for `read(2)`; mappings cache only for their lifetime. See [memory.md](../kernel/memory.md#the-unified-page-cache) |
| `pcache.max_mb=N` | half of RAM | page-cache cap |
| `usbms.wryield=0` | on | one FIFO disk queue instead of reads-first ([pc.md](../hardware/pc.md#reads-before-writes-and-the-slow-stick-emulation)) |
| `usbms.delay_us=N`, `usbms.wdelay_us=N` | 0 | add N us to every read / write command (slow-stick emulation) |
| `usbms.au_kib=K`, `usbms.au_open=N`, `usbms.merge_us=M`, `usbms.au_hit_us=H` | off; H = 4000 | allocation-unit merge model: a write outside the N most recently written K-KiB units costs M us, inside costs H |

### Limitations

- **No `S_IFSOCK`.** AF_UNIX sockets cannot be bound on f2fs; use a tmpfs path ([vfs.md](vfs.md#tmpfs)).
- **Symlinks cannot leave the volume.** An absolute target naming a namespace the VFS intercepts (`/tmp`, `/dev`, `/proc`) resolves inside the volume and answers `ENOENT` — never an escape. The other direction (a tmpfs symlink naming an f2fs path) works.
- Files created at runtime before timestamps existed show `Jan 1 00:00`.
- One block cache and one checkpoint per volume: a checkpoint still writes all dirty state of the volume, so `fsync` latency scales with it.
- **No roll-forward fsync and no NAT/SIT journaling.** An fsynced file is durable only through a full checkpoint; Linux's fsync-node log (roll-forward recovery) and the NAT/SIT journal in the checkpoint's compact summary are not implemented (listed in `TODO.md`). That is why lazy fsync has a data-loss window rather than losing a few records.
- The `with_mount` retry model needs every state change of a request to be undoable; new request kinds must route through it.

## Extended attributes and POSIX ACLs

Xattrs and ACLs are **enforced end to end**. [`servers/xattr`](../../servers/xattr) is the single source of truth, shared verbatim by the kernel's syscall layer, tmpfs and f2fs:

- size caps (`XATTR_NAME_MAX` 255, `XATTR_SIZE_MAX` 64 KiB; arenas of 2 KiB per tmpfs inode and 4076 bytes per f2fs xattr node);
- namespace indices (`user.`, `system.posix_acl_access`, `system.posix_acl_default`, `trusted.`) and their permission gates (`may_read_xattr`, `may_write_xattr`);
- the packed entry codec, in the Linux `f2fs_xattr_entry` layout, so a blob written by one filesystem is interpretable by the other;
- the POSIX 1003.1e ACL evaluator (`access_check`, `may_access`) with ACL wire-format validation;
- `acl_create`, Linux's `posix_acl_create`: a child of a directory with a default ACL inherits it (with the owner/mask/other entries intersected with the requested mode), a child directory also inherits the default ACL itself, and the umask is not applied in that case. The umask therefore travels with the create mode (`pack_create_mode`) so each filesystem applies it through the same function.

Two invariants make `stat`, `ls`'s `+` and `getxattr` agree: **trivial ACLs are never stored** (they fold into the mode), and **when an ACL exists the group mode bits equal its mask**. `chmod` rewrites a stored ACL's mask accordingly.

The decision is that **every permission check calls the crate's evaluator** — `open`, the path walk, creation, removal, `faccessat` (`VFS_ACCESS`), AF_UNIX `connect` — never an ad-hoc mode-bit test. That is why ACLs kept working when traversal and creation checks were added later. See [security.md](security.md#filesystem-permission-enforcement).

`/bin/xattr` ([`userland/xattr-util`](../../userland/xattr-util)) lists, gets, sets and deletes string values; uutils `ls -l` prints `+` for any non-empty `listxattr`, not only ACLs.

## Block devices

[`servers/vfs/src/block.rs`](../../servers/vfs/src/block.rs) presents disks the way Linux does, because everything a block device is to userspace is a VFS question: a node in `/dev`, a subtree of `/sys`, an fd with an offset, and a handful of ioctls.

### The two registries

- **`drivers::blkdev`** ([`drivers/src/blkdev.rs`](../../drivers/src/blkdev.rs), trait in [`drivers/src/blockdev.rs`](../../drivers/src/blockdev.rs)) is the runtime registry of physical disks and kernel-scanned partitions: 64 append-only slots of `&'static dyn BlockDevice`. Whole disks are named `<class><letter>` (`vda`, `sda`) with a counter per class; partitions are `<disk>N`, or `<disk>pN` when the disk name ends in a digit. Each virtio disk is its own object with its own lock and 512-byte sector I/O — before this, one global lock was held across every request on every disk. The old 4 KiB block façade (`read_block`, `write_block`, `flush`, `has_f2fs`, …) is kept and adds the partition offset and an end-of-partition bound.
- **The VFS block registry** (`BDEVS`, 48 slots) mirrors it lazily — every lookup or enumeration first syncs if `disk_count()` has grown, so a disk registered at runtime appears in `/dev` and `/sys/class/block` on the next lookup — and adds what only userspace creates: eight loop devices and BLKPG-added partitions. Raw I/O crosses back into the kernel through the `&'static DiskOps` vtable registered at boot (`describe` returns name, parent, partition number, start, size, logical block size, removable and SCSI flags without doing I/O).

Major numbers are 254 for `vd` and 8 for `sd`; minor = ordinal × 16 + partition number.

### Partition scanning

[`drivers/src/partscan.rs`](../../drivers/src/partscan.rs) is a pure parser over a read callback, run once all virtio disks have registered (so `vda`/`vdb`/`vdc` keep indices 0/1/2):

- **MBR**: primary entries only; superfloppy FAT boot sectors are rejected by the `0x00`/`0x80` boot-flag rule; extended containers are skipped.
- **GPT**: found through a protective `0xEE` entry; header CRC, `my_lba` and entry-array CRC are checked, falling back to the backup header at the last LBA; both bad means no table.
- PARTUUID text forms for both (GPT mixed-endian, MBR `ssssssss-pp`).
- A partition that does not start on a 4096-byte boundary never reports f2fs.

### What userspace sees

The contract was set by disks-rs, which needs no udev, no `/proc` and no `readlink` — only:

- `/sys/class/block/<name>/{size,partition,start,dev,removable,queue/logical_block_size,device/model,loop/backing_file}` via `read_dir` + `read_to_string`, with partitions listed both at the top level and inside their disk's directory. `size` and `start` are always in 512-byte sectors regardless of the logical block size, because consumers multiply straight back by 512.
- `/dev/loop-control` with `LOOP_CTL_GET_FREE`, and `/dev/loop0..7` with `LOOP_SET_FD`, `LOOP_SET_STATUS64`, `LOOP_CLR_FD` (eight nodes exist whether or not attached, Linux's `max_loop` default).
- BLKPG add/delete partition (in bytes), `seek(End(0))` for device size, and `fsync` on the block fd.

Block ioctls are routed by **fd kind**. A loop device takes its **own** open of the backing file, so the backing fd can be closed. Block-device opens require `root:disk 0660` (enforced at `open` and `access`); before that, uid 1000 could read `/dev/vdb` raw.

`lsblk` ([`userland/lsblk`](../../userland/lsblk)) asks the kernel for registry names through `SYS_BLKDEV_NAME` (523), so it prints `/dev/vda1` rather than a name derived from the slot index.

### Design decisions

- **Synthesise sysfs, never store it.** Materialising `/sys/class/block` would cost tmpfs slots and create a coherence problem: every BLKPG change, every `LOOP_SET_FD`/`LOOP_CLR_FD` and every `ftruncate` of a backing file would have to rewrite the tree, and any path that forgot would serve a stale `size`. Directories are `VnodeKind::SysBlock { dev, level }` — no path string, the pair *is* the identity — and attribute files are generated into an ephemeral slot at open time.
- **Own only `/sys/class/block`.** See the black-desktop regression in [vfs.md](vfs.md#routing-a-path).
- **Leaf locks only.** `BDEVS` and `LOOPS` are never held across a call out of the module: loop I/O re-enters the VFS on the same thread, so every path copies what it needs, drops the lock, then does I/O.
- **No yielding inside block I/O.** Syscalls run with interrupts off under IRQ-off spinlocks, so a yield there deadlocks; USB I/O spins with preemption disabled, like virtio.
- **An fd's offset belongs to the fd.** `VnodeKind::BlockDev { dev, pos }` keeps the offset per descriptor: two opens of `/dev/loop0` seek independently, and the `gpt` crate sizes a device by seeking its own handle to the end while the partitioner holds another.

### Formatting

`ports/mkfs-fat` (FAT32 through the `fatfs` crate) and `ports/mkfs-xfs` (a from-scratch XFS v5 writer whose superblock is byte-identical to xfsprogs and passes `xfs_repair -n`; it refuses volumes under 300 MiB or 2 AGs, and must not set `XFS_SB_VERSION2_FTYPE` on v5) back disks-rs's `mkfs.fat` and `mkfs.xfs` calls. `disktester` (run as `cd /root/disks-rs && disktester`, since it loads its KDL relative to the cwd) creates a 32 GiB sparse image, attaches a loop device, writes GPT, BLKPG-syncs three partitions and formats them. See [../userland/ports.md](../userland/ports.md).

### /dev/root and booting from USB

`mount("/dev/root")` is resolved by the kernel, in order:

1. `root=/dev/<name>` or `root=PARTUUID=…` on the kernel command line (read through Limine's executable-cmdline request). An explicit `root=` that matches nothing is `ENODEV`; the kernel does not guess.
2. Index 1, if it is a whole `vd` disk carrying the f2fs magic (the QEMU layout).
3. The unique removable or `sd*` disk or partition with f2fs magic, ties broken first by the boot disk (Limine's GPT GUID or MBR signature), then by partition type `0x83` / the Linux filesystem GUID. Two candidates are `ENOTUNIQ` with a log line listing them.

`init` mounts `/dev/root`, retrying on `ENODEV` every 100 ms for up to 30 s (a USB stick enumerates late), and falls back to `/dev/vdb` on `EINVAL` (an older kernel) or timeout. Success logs `[BLK] /dev/root -> <name>`.

A USB mass-storage stick registers through the same `blkdev::register` call: the BOT/SCSI layer ([`drivers/usb/src/msc`](../../drivers/usb/src/msc)) implements CBW/CSW framing, BOT reset recovery, READ/WRITE(10) or (16) as the LBA requires, READ CAPACITY(16) fallback, 64 KiB chunking, `SYNCHRONIZE CACHE` for `flush`, a three-attempts / reset / three-attempts error ladder, and a 10 s `TEST UNIT READY` wait; its host-side tests run a mock device with fault injection. A stick written by [`scripts/write-usb.sh`](../../scripts/write-usb.sh) carries `root=PARTUUID=` in its `limine.conf`, and `run-qemu.sh <arch> --usb-stick[=gpt]` boots one in QEMU. Root-on-USB has been booted on a real x86_64 PC. The xHCI side and the hardware story are in [../hardware/pc.md](../hardware/pc.md).

### Limitations

- No unplug: registry entries are permanent and a pulled stick keeps its index. 64 entries total.
- Two concurrent `register` calls can interleave partition indices (names and parents stay correct).
- Nothing mounts FAT or XFS; there is no reader for either.
- BLKPG and loop ioctls are gated only by the device open (Linux additionally wants `CAP_SYS_ADMIN` for BLKPG).
- On the Raspberry Pi the SDHCI path is compile-time and does not support runtime registration.

## Testing notes

- `pcachetest` (page-cache coherency, in the `regress` suite) and `pagecache-crashtest.py` (fsync ordering across a SIGKILL of QEMU), `fsbench` (read, walk, mmapdir, tick, statlat, dbwrite, statfs, statwalk, stallprobe) and `killmt` fs_io/fs_exit are described in [../development/testing.md](../development/testing.md).
- `f2fstest` covers reclaim, truncate, `statfs` motion and re-extension; `vfstest` covers the VFS-facing behaviour, xattrs and ACLs on both filesystems; `permtest` covers default-ACL inheritance; `disktester` covers the block layer end to end; `drivers/partscan-tests` (14 host tests) and `drivers/usb/msc-tests` (24 host tests) cover the parsers.
- **The data volume is persistent and is attached read-write from the repository root, so it survives reboots.** Tests that leave residue there (`vfstest`'s `xattr_list_f2fs`, which creates a file with `O_TRUNC` — clearing data, never xattrs — and then asserts an empty `listxattr`) fail on a second run against the same image on *either* architecture. This was long recorded as an "aarch64-only f2fs bug"; it was a dirty image. Judge f2fs results on a fresh image from `build-all.sh`, and remember that `build-all.sh` regenerates the images, wiping anything written before a rebuild. See [../development/testing.md](../development/testing.md).

## Code

- [`servers/f2fs/src/lib.rs`](../../servers/f2fs/src/lib.rs) — the filesystem server.
- [`servers/xattr/src/lib.rs`](../../servers/xattr/src/lib.rs) — the xattr/ACL/permission contract.
- [`servers/vfs/src/block.rs`](../../servers/vfs/src/block.rs) — `/dev` block nodes, loops, BLKPG, `/sys/class/block`.
- [`drivers/src/blkdev.rs`](../../drivers/src/blkdev.rs), [`drivers/src/partscan.rs`](../../drivers/src/partscan.rs), [`drivers/src/virtio_blk.rs`](../../drivers/src/virtio_blk.rs), [`drivers/src/usb_storage.rs`](../../drivers/src/usb_storage.rs) — the physical side.
- [`scripts/mkfs-f2fs-populated.py`](../../scripts/mkfs-f2fs-populated.py) — builds the images, seeds accounts and the on-disk `/sys` stubs.

## See also

- [vfs.md](vfs.md) — routing, tmpfs, mounted-file proxying.
- [security.md](security.md) — permission enforcement built on the xattr evaluator.
- [../hardware/pc.md](../hardware/pc.md) — xHCI and real-hardware USB root.
- [../hardware/raspberry-pi-5.md](../hardware/raspberry-pi-5.md) — SD storage on the Pi.
