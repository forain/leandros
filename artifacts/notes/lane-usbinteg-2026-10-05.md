# lane/usbinteg — 2026-10-05: root f2fs on an xHCI USB stick

Branch `lane/usbinteg`, base main `7d114ce`. Not pushed. Mac (aarch64 host):
aarch64 runs under HVF, x86_64 under TCG, GPU QEMU `~/.local/qemu-gpu-gles31` (virgl).

**Result:** `./scripts/run-qemu.sh <arch> --usb-stick[=gpt]` boots LeandrOS
from the stick with the stick as root, on both arches. It reaches the graphical
login, and the COSMIC desktop and cosmic-term come up. Default boots are
unchanged.

## What was merged

- `lane/blkroot` was merged first (fast-forward). After it: `lane/xhci`,
  `lane/usbmsc` and `lane/usbharness`.
- No textual conflicts.
- The harness lane's `scripts/write-usb-x86_64.sh` wrapper is the canonical
  one.

## New in this lane

| commit | what |
|---|---|
| `6e0756a` | Dedupe: `drivers/usb/src/transport.rs` is the only `UsbErr`/`BulkTransport`. `msc::transport` re-exports it. `msc-tests` includes the file by `#[path]`. |
| `02219e2` | `drivers/src/usb_storage.rs` glue, plus the **poll-yield deadlock fix** (below) and `sched::preempt_disabled()`. |
| `8acfcaa` | `write-usb.sh` writes `cmdline: root=PARTUUID=…` into the stick's own `limine.conf`. Adds `--no-cmdline`. Adds `LEANDROS_USB_IMAGE` (run-qemu) and `LEANDROS_USB_STICK=a.img[,b.img]` (driver.py). |
| `d39f97c` | **Boot-disk tie-break fix:** read Limine's `limine_file` at protocol offsets (below). Logs `[BLK] boot disk: …`. |
| `b2f8471` | init: on ENOTUNIQ, print what to do and park in `usleep`; it no longer spins in `sched_yield`. |
| `91dbf36` | driver.py: stick device ids `usbstickN`, so a test can unplug one with QMP `device_del`. |

### usb_storage glue

`drivers::usb_storage::init()` is called in `kernel/src/init.rs` right after
`usb_hcd::init()`, which itself runs after `blkdev::init()` and before
userspace init is spawned. In the logs the stick is therefore registered
before init's first `/dev/root` attempt, so the 30 s retry is never needed.

For each `XhciBulk` it does:
1. `ScsiDisk::new(t, delay_ms)`. `delay_ms` parks on the scheduler's timed
   wait channel, like `nanosleep`.
2. `init()`.
3. Logs `[USB-MSC] interface I LUN L: N x S-byte sectors`.
4. Boxes a `UsbDisk { Mutex<ScsiDisk> }` and leaks it.
5. `blkdev::register()` → `sda`, then partitions `sda1`, `sda2`…

How `UsbDisk` behaves:
- It reports `name_class "sd"` and `removable true`.
- A write-protected medium refuses writes with `BlkErr::Io`; this is logged
  once. `flush` is a no-op on such a medium.
- Every `MscErr` maps to `Io` (EIO). `InvalidArg` maps to `OutOfRange`.
- `blkdev::read_blocks` hands the whole multi-block request to
  `read_sectors` in one call. `ScsiDisk` then sends it as READ(10) commands
  of up to 64 KiB each (`MAX_CHUNK_BYTES`). The cold-read numbers below show
  that is enough, so the chunk size was not raised.

### Bug 1: whole-guest hang on the first USB-root boot (fixed)

**Symptom.** The boot reached `Starting getty loop...`, then the guest hung.
QEMU used 400 % CPU, Ctrl-T on serial did nothing, and there were no
`[WDOG]` lines.

**Cause.**
- This kernel runs syscalls with IRQs masked, and every `spin::Mutex` is an
  IRQ-off spinlock.
- The xHCI poll hook `usb_hcd::relax()` called `sched::yield_now()`, and it
  did so in the middle of f2fs/VFS block I/O, while those layers held their
  locks.
- The task that held a lock got parked. The next task that wanted the lock
  spun at IF=0 forever, and eventually all four CPUs did.
- This is the same hazard `sched::PREEMPT_DISABLE` documents for
  virtio-gpu. virtio-blk never hit it because it spins and never yields.

**Fix.**
- `relax()` yields only when IRQs are enabled, which is boot-time
  enumeration in the kernel init task.
- With IRQs masked it spins.
- Inside the preempt-disabled `IoWindow` that each block request opens, it
  also calls `irq_window()` every 256 polls. The tick, the tick hooks and
  poll deadlines keep running, but no task switch happens. This is
  virtio-gpu's `SpinWindow` contract.
- After the fix: all boots pass, `regress` passes on a USB root, and the
  desktop comes up.

### Bug 2: boot-disk tie-break never worked (fixed)

The `limine` crate 0.6.3 has two problems in this area:
- Its `File` struct leaves out the protocol's `unused` and `tftp_port`
  words. Its `mbr_disk_id` field therefore reads `tftp_port`, which is
  always 0.
- Its `Uuid` is not `repr(C)`, so the GPT disk GUID came back scrambled. The
  first 8 bytes were right and the last 8 held `partition_index` and
  `mbr_disk_id`.

Found from a raw dump of Limine 11.4.1's response:
`… 01000000 d206b320 | 4a945056 7220ce43 be0df05f e236797b`, which is
`partition_index` = 1, `mbr_disk_id`, then the disk GUID at offset 64.

Effect: two distinct f2fs sticks without `root=` were reported as ambiguous
even when one of them was the boot disk. The fix reads `media_type` at
offset 40, `mbr_disk_id` at 60 and `gpt_disk_uuid` at 64 directly from the
response. This was the blkroot note's open risk: "ExecutableFile boot-disk
tie-break was not observed live".

### PARTUUID on the stick

- `write-usb.sh` generates the identifiers itself and patches only the
  stick's copy of `limine.conf`. The repo file is untouched.
- MBR: `%08x-02` from the disk signature. The signature is forced non-zero,
  and the script re-reads it after `limine bios-install` and dies if it
  changed.
- GPT: the root partition's unique GUID, as lower-case mixed-endian text.
- Both forms are exactly what `partscan::PartUuid::format` produces, and
  matching is case-insensitive (verified with an upper-case
  `846AC9BA-02`).

## Test matrix (final tree, full `build-all.sh` release both arches, RC=0)

| test | aarch64 (HVF) | x86_64 (TCG) |
|---|---|---|
| `--usb-stick` (MBR), run-qemu.sh, headless | PASS → graphical login, virgl contexts | PASS → graphical login, virgl contexts |
| `--usb-stick=gpt`, run-qemu.sh, headless | PASS | PASS (sda3 = BIOS boot partition) |
| ffsession `--nofirefox` on MBR stick | greeter 0.7 / desktop 0.8 / term 0.7 s, total 36.1 s | 1.2 / 14.0 / 1.2 s, total 64.9 s |
| ffsession on GPT stick | 0.7 / 0.7 / 0.7 s, total 35.5 s | 1.2 / 15.1 / 1.6 s, total 96.8 s (one sample) |
| ffsession default (virtio) | 0.7 / 10.7 / 0.7 s, total 35.6 s | 1.4 / 15.1 / 1.8 s, total 67.5 s |
| persistence: 32 MiB random file + md5, sync, `poweroff`, reboot same image | PASS (MBR; `checkpoint ver 992 is a clean unmount`, `md5sum -c` OK) | PASS (GPT; ver 793 clean, OK) |
| no cmdline, single stick (removable fallback) | PASS `/dev/root -> sda2` | — |
| no cmdline, two distinct sticks (boot-disk tie-break) | PASS, GPT boot stick → sda2 | PASS, MBR boot stick → sda2 |
| no cmdline, two identical clones (same disk id) | ENOTUNIQ: kernel lists both candidates; init prints the remedy and parks | — |
| cmdline names the NON-boot stick | PASS `root=PARTUUID=846AC9BA-02` → `sdb2` | — |
| raw whole-disk f2fs stick as root (`root=/dev/sdb`) | PASS `/dev/root -> sdb` | — |
| surprise removal of a non-root stick (QMP `device_del`) | `[USB-MSC] read failed: Transport(Disconnected)`, `dd: IO error` in 18 ms; root keeps working | — |
| `runtests regress` on a USB root | 15/15 PASS | — |
| default boot: `[BLK] vda/vdb/vdc`, `/dev/root -> vdb`, `/data` on vdc | unchanged | unchanged |
| default `runtests regress` | 15/15 PASS | 15/15 PASS |
| default disktester | DT_RC=0, Format success, `To mount: "/dev/loop0p3" as Root` | DT_RC=0, same |
| msc host tests (`cargo test --manifest-path drivers/usb/msc-tests/Cargo.toml --target aarch64-apple-darwin`) | 24/24 | |
| partscan host tests (`drivers/partscan-tests`) | 14/14 | |

Checkpoint versions above 1 and "NOT a clean unmount" lines in some of the
ad-hoc runs come from `f2fs-data0-<arch>.img` itself. That image is the
mutable virtio root, which regress runs and killed (`driver.py stop`) boots
dirty, and `write-usb.sh` copies whatever state it is in. They are not stick
corruption: the freshly built sticks all show `checkpoint ver 1 is a clean
unmount`.

Serial evidence, aarch64 GPT stick, final build:

    [BLK] kernel cmdline: root=PARTUUID=e024a7da-d17e-4ff3-942c-d506bab981ed
    [BLK] boot disk: GPT disk GUID aed223d6-6ecf-4af4-afb4-8d712bcaa33b
    [USB] Bus 001 Device 001: ID 46f4:0001 QEMU QEMU USB HARDDRIVE [port 1, super-speed (5G), class 08, bulk-only mass storage]
    [USB-MSC] interface 0 LUN 0: 8179712 x 512-byte sectors
    [BLK] /dev/sda: 8179712 x 512-byte sectors, removable
    [BLK] /dev/sda1: start 2048, 1048576 sectors
    [BLK] /dev/sda2: start 1050624, 7127040 sectors, linux
    [BLK] /dev/root -> sda2
    [F2FS] /dev/sda2: checkpoint ver 1 is a clean unmount
    pivot_root successful! Root is now F2FS.
    graphical login started (greetd, pid 5), log at /var/log/greetd.log

Ambiguous root (two clones, no cmdline):

    [BLK] /dev/root is ambiguous between: /dev/sda2 /dev/sdb2 (pass root= on the kernel command line)
    mount /dev/root failed (errno 76)
    More than one disk could be the root filesystem (see the [BLK] line above).
    Remove the extra USB disks, or put root=PARTUUID=<id> or root=/dev/<name>
    in the boot entry's cmdline (write-usb.sh does this by default).

## Boot time (QEMU launch → serial `login:`, `driver.py`, no GPU; mean of 2–3 runs)

| | virtio root | USB root (MBR) | USB root (GPT) |
|---|---|---|---|
| aarch64 HVF | 0.40 s | 0.88 s | 0.87 s |
| x86_64 TCG | 2.88 s | 3.31 s | 3.32 s |

- The HVF numbers look implausibly small, so they were cross-checked with
  `/proc/uptime` read right after the prompt: 0.7 s (virtio) and 1.0 s
  (USB).
- About 0.2 s of the USB cost is firmware. OVMF/AAVMF load Limine and the
  kernel over USB more slowly than over virtio.
- About 0.3 s is the kernel, from `[INIT] Kernel init task` to `[BLK]
  /dev/sda2`. Almost all of it is the xHCI driver's fixed 100 ms debounce
  plus its 200 ms second enumeration pass.
- Nothing pathological, so no transfer or IRQ work was done. A cheap win if
  ever wanted: skip the +200 ms second pass when the first pass already
  found every connected port.
- Cold sequential read of the 390 MB `/bin/mame` (aarch64): 0.29 s on virtio,
  0.38 s on USB. 64 KiB SCSI commands are adequate.

## Remaining risks (real hardware; carried from the xhci note plus new ones)

- **No hub support.** Devices behind a hub are not enumerated. That includes
  the internal rate-matching hubs that put many laptops' external USB2 ports
  behind a hub. This is the biggest functional gap.
- **Firmware DMA window.** A controller left running by firmware keeps
  DMAing into memory the kernel may already have reclaimed, until
  `usb_hcd::init()` stops it. The early quiesce is not done.
- **Not exercised by QEMU:**
  - 64-byte contexts (CSZ=1)
  - scratchpad buffers
  - the BIOS handoff / SMI disable
  - AC64=0 (only simulated by forcing dma32)
  - a firmware-left-running controller
  - a Stop Endpoint after a real NAK timeout
  - command-ring abort
- **Polled I/O spins with IRQs masked.** A request stalls its CPU until it
  completes, as virtio-blk does. Ticks still arrive through `irq_window`.
  A stick that times out (5 s per transfer, 30 s per SYNCHRONIZE CACHE, up
  to 6 attempts plus a reset in the msc error ladder) holds that CPU in the
  syscall for that long; `[WDOG]` will not fire, because ticks are taken.
  Surprise removal is fast, since `Disconnected` is not retried.
- **Slow USB storage at boot.** Media that are not ready make
  `usb_storage::init()` block the kernel init task for up to about 10 s per
  LUN in `ScsiDisk::init`. It runs inline, before userspace init. A
  background kernel thread would avoid this, but kernel tasks have no clean
  exit path today.
- **No hot-plug after boot, no unplug path in the registry.** A stick
  removed while mounted gives EIO, and its `sdX` entry stays.
- **Cache coherency.** `DmaBuf` sync is barrier-only. That is fine on x86
  and QEMU virt, but wrong for a non-coherent aarch64 board. The Pi 5 has
  no PCIe/RP1 bring-up anyway.
- **UAS-only devices are skipped** (BOT only), and so are the AMD, ASMedia
  and VIA xHCI quirks.
- **Limine crate.** Any other use of `limine::file::File`'s
  `mbr_disk_id`/`*_uuid` fields is wrong in the same way. Upgrading the
  crate (or patching it) would let `capture_boot_identity` go back to the
  typed struct.

## Commands

    ./scripts/run-qemu.sh aarch64 --usb-stick          # MBR stick, root=PARTUUID in its limine.conf
    ./scripts/run-qemu.sh x86_64  --usb-stick=gpt
    ./scripts/write-usb.sh <arch> --image s.img [--gpt] [--no-cmdline]
    LEANDROS_USB_IMAGE=s.img ./scripts/run-qemu.sh <arch> --usb-stick
    LEANDROS_USB_STICK=a.img,b.img python3 .claude/skills/run-leandros/driver.py start <arch>
    python3 .claude/skills/run-leandros/driver.py qmp device_del '{"id": "usbstick1"}'
