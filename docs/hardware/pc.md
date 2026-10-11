# Real PCs (x86_64 UEFI)

A real x86_64 PC boots LeandrOS from a USB stick: UEFI firmware loads Limine from the stick, the
kernel finds its F2FS root on the same stick through its own xHCI, bulk-only-transport and SCSI
drivers, and a USB keyboard logs in. On an NVIDIA RTX 3070, OpenRM — NVIDIA's open resource manager,
linked into the kernel — boots the GPU's GSP firmware, a nouveau-compatible DRM interface lets
upstream Mesa NVK and Zink render on it, KMS scans out by copy-engine blits, and the COSMIC desktop
runs with Firefox playing YouTube with sound over a Realtek Ethernet NIC and Intel HD Audio. The
remaining work on the PC is performance and robustness, not bring-up (see [Limitations](#limitations-and-open-work)).

## Status (2026-10-11)

| Capability | State | Since |
|---|---|---|
| UEFI boot from a USB stick, root F2FS on the stick | **Works on real hardware** | QEMU `8177771` (2026-10-05); PC 2026-10-07 |
| USB HID boot keyboard and mouse | **Works on real hardware** (low-speed Logitech keyboard and mouse) | `0fff571`; PC 2026-10-08 |
| Kernel log persisted on the stick | Works, with journal pages and a cadence that backs off on a slow stick | `d4d2e9f`; `7108096` |
| All CPUs online on sparse APIC IDs | Works (4 CPUs, APIC IDs 0,2,4,6) | `963c681` |
| `poweroff` through ACPI S5 | Works | — |
| NVIDIA RTX 3070: GSP-RM `INIT_DONE` | **First try**, 3.5 s | `95bd575`; PC 2026-10-08 |
| NVIDIA: in-kernel copy-engine self-test (G3) | Passed on the 3070 | 2026-10-08 |
| NVIDIA: Vulkan through Mesa NVK (`nvktest`: info, fill, timeline, compute, triangle, depth, timestamps) | **8/8 and a triangle on the 3070** | `c41deda`; boots 3 and 4 |
| NVIDIA: KMS on card0 (`nvkmstest`) | **6/6 on the PC** | `698857a`; boot 5 |
| NVIDIA: OpenGL ES through Zink on NVK (`nvgltest`) and the COSMIC session | **8/8; the session ran on the 3070** | `64fbee3`, `a15c6d7`; boot 6 |
| Realtek RTL8111/8168 Ethernet | **Works** (DHCP, DNS, TCP; 77 and 166 Mbit/s in the first measurements) | `1c119a9`, `fb37c94`; boot 8-9 |
| Intel HD Audio playback | **Works** | `71f0d9a`; boot 8 |
| Desktop, Firefox playing YouTube with sound on the GPU | **Works** | boot 11-12 |
| USB hubs | Not supported (work started, paused) | — |
| Wi-Fi | No driver | — |

The test machine is an Intel Kaby Lake desktop (4 cores, 4 threads, APIC IDs 0,2,4,6, no integrated
GPU in use), a Sunrise Point PCH, a Realtek RTL8111 NIC, an ASMedia PCIe bridge, and an ASUS RTX 3070
(GA104, device `10de:2484`) with Resizable BAR **off**, so the UEFI GOP framebuffer (1024×768, pitch
4096) sits at the base of the GPU's 256 MiB BAR1 at `0xE0000000`.

## Booting from a USB stick

### Writing the stick

```sh
./scripts/build-all.sh                                  # release, both arches
./scripts/write-usb.sh x86_64 /dev/diskN               # macOS: see `diskutil list external`
./scripts/write-usb.sh x86_64 /dev/sdX --gpt            # Linux; GPT instead of MBR
./scripts/write-usb.sh aarch64 --image stick.img        # a raw image instead of a device
```

`scripts/write-usb.sh <arch> (<device> | --image <out.img>) [--yes] [--gpt] [--no-cmdline]` writes
Limine (UEFI `BOOTX64.EFI` plus the BIOS stage on x86_64, `BOOTAA64.EFI` on aarch64), the kernel,
the initrd, and the populated F2FS root:

| | MBR (default) | GPT (`--gpt`) |
|---|---|---|
| p1 | 512 MiB FAT32 ESP, type `0xEF`, active | 512 MiB EFI System partition |
| p2 | F2FS root, type `0x83` | F2FS root, Linux filesystem data type |
| log | 64 MiB raw, type `0xDA` | 64 MiB raw, type `042B7470-…` named `LEANDROS-LOG` |
| x86_64 only | — | a ~992 KiB BIOS boot partition (`EF02`) at LBA 64..2047, listed last |

Limine's BIOS installer refuses a GPT disk without a BIOS boot partition, hence the extra entry; it is
listed last so the ESP and root stay partitions 1 and 2. The MBR scheme is the same as the Pi 5 SD
card's, so the root is found the same way on both media.

The script generates fresh disk identifiers and writes **`cmdline: root=PARTUUID=…`** into the
stick's own copy of `limine.conf` (the repository file is untouched); `--no-cmdline` leaves the
choice to the kernel. `LEANDROS_USB_EXTRA_CMDLINE` appends further kernel arguments. The f2fs root
image is about 3.4 GiB, so a stick image is a ~4 GiB sparse file. Two host traps the script handles:
macOS BSD `dd conv=sparse` truncates the output at the last block written (which cut the backup GPT
off the image), and the MBR disk signature is forced non-zero and re-checked after
`limine bios-install`.

**Sparse flash.** Writing to a device is sparse by default (`scripts/sparse-write.py`; lane sparseflash). Cheap flash treats every block it was ever written as live, so `dd`-ing the zeros of a mostly empty image (the 512 MiB ESP, the f2fs free tail, the 64 MiB log partition) leaves the controller no erased blocks and later random writes crawl. The script writes only the non-zero 4 KiB blocks (runs, with gaps under 1 MiB bridged) plus the ranges that must be written even when zero because they are read back: the first MiB (MBR/GPT; zeros erase a stale GPT) and the last 34 sectors, the ESP's reserved area, both FATs and every allocated cluster, f2fs everything below `main_blkaddr` (superblock, checkpoints, SIT, NAT, SSA) and each main block the image's SIT marks valid, and the 4 KiB header page of each log slot. Skipped: the ESP's free clusters, the f2fs free main area (the server zeroes every data block it allocates and writes node blocks whole) and the rest of the root partition. `--full` restores the plain `dd`; for `--image`, `--sparse` writes onto an existing image file instead of replacing it.

`scripts/write-usb-x86_64.sh` is a thin wrapper kept for compatibility. `scripts/deploy-x86_64.sh` is
an older script that refers to a runner which no longer exists; use `write-usb.sh`.

### Choosing the root: `/dev/root`

The kernel resolves `/dev/root` in this order (`cab4ee0`, `d39f97c`):

1. `root=/dev/<name>` or `root=PARTUUID=…` on the kernel command line (matching is case-insensitive).
   An explicit `root=` that matches nothing gives `ENODEV`; the kernel does not guess.
2. Index 1, if it is a whole `vd` disk with F2FS magic — the QEMU default layout.
3. The unique removable or `sd*` disk or partition with F2FS magic. Ties are broken first by the
   **boot disk** (Limine's GPT disk GUID or MBR signature), then by partition type `0x83` / Linux
   filesystem GUID.

Nothing found is `ENODEV`; more than one candidate is `ENOTUNIQ` (errno 76) plus a `[BLK]` line
listing them. Init retries `ENODEV` every 100 ms for up to 30 s (for disks that appear late), falls
back to `/dev/vdb` on `EINVAL` (an old kernel) or timeout, and on `ENOTUNIQ` prints what to do
("remove the extra USB disks, or put `root=PARTUUID=<id>` … in the boot entry's cmdline") and parks.

The boot-disk tie-break needed a workaround: the `limine` crate 0.6.3's `File` structure omits two
protocol words and its `Uuid` is not `repr(C)`, so `mbr_disk_id` read the always-zero `tftp_port`
and the GPT GUID came back scrambled. The kernel now reads `media_type` (offset 40), `mbr_disk_id`
(60) and `gpt_disk_uuid` (64) from the raw response. Any other use of those crate fields is wrong in
the same way.

The block registry, partition scanner and `/dev/root` code are shared with QEMU; see
[servers/filesystems.md](../servers/filesystems.md) for the VFS side and [drivers.md](drivers.md#block-devices)
for the driver side.

### Testing the stick path in QEMU

```sh
./scripts/run-qemu.sh aarch64 --usb-stick           # MBR stick
./scripts/run-qemu.sh x86_64  --usb-stick=gpt
LEANDROS_USB_IMAGE=s.img ./scripts/run-qemu.sh x86_64 --usb-stick
```

`--usb-stick` (UEFI boots only) builds `leandros-usb-<arch>-<mbr|gpt>.img` when its inputs are
newer and attaches **only** `qemu-xhci` + `usb-storage` — no virtio disks — so the firmware, Limine
and the kernel all have to find everything over USB, as on the PC. The test harness can attach
several sticks (`LEANDROS_USB_STICK=a.img,b.img`) and unplug one over QMP. On both architectures the
stick boots to the graphical login and a COSMIC session in QEMU; surprise removal of a non-root
stick returns `EIO` in 18 ms while the root keeps working; data written before `poweroff` survives
a reboot with a clean checkpoint. Boot to `login:` costs about 0.3 s more than virtio, almost all of
it the xHCI driver's fixed 100 ms debounce plus its 200 ms second enumeration pass. A cold read of a
390 MB binary takes 0.38 s from the stick versus 0.29 s from virtio, so 64 KiB SCSI commands were
judged adequate.

## The USB stack

The `usb` crate ([drivers/usb](../../drivers/usb/src/lib.rs)) holds the host-controller and class
drivers; [drivers/src/usb_hcd.rs](../../drivers/src/usb_hcd.rs),
[usb_storage.rs](../../drivers/src/usb_storage.rs) and [usb_hid.rs](../../drivers/src/usb_hid.rs) glue
them into the kernel. `init()` finds every PCI function of class `0C/03/30` on both architectures and
returns after one cached lookup when there is none — every default QEMU configuration.

### xHCI host controller

The July 2026 driver corrupted kernel memory "some time after bring-up", and for months that was
filed as a timing-sensitive hardware interaction. The cause was mundane: `usb::xhci::Xhci` was a
**573,568-byte** value (256 inline slots) returned by value onto a 128 KiB kernel stack with no
guard page. Rust has no placement-new, so even `Box::new(Xhci::new())` materialised it on the stack
first, writing ~445 KiB below the stack into whatever buddy blocks sat there. The stack-frame check
in `scripts/check-stack-frames.py` (48 KiB budget) did not exist then; it would have refused that
kernel. The driver was rewritten (`2c2d152`) and now pins its size with compile-time assertions
(`size_of::<Xhci>() < 1024`, `size_of::<Slot>() < 512`). The rewrite also fixed DMA into freed pages
after a timed-out command, an unwritten `DCBAA[slot]` (the controller wrote device contexts to
physical address 0), stack buffers used as DMA targets, an event ring whose consumer wrapped at 255
of 256, and a dozen protocol errors.

Design of the current driver (`drivers/usb/src/xhci/`):

- **Bring-up**: USBLEGSUP BIOS-to-OS handoff (1 s, then forced) with SMI disable; Supported Protocol
  capabilities map each port to USB2 or USB3; a controller the firmware left running is stopped;
  HCRST; `MaxSlotsEn = min(HCSPARAMS1, 64)`; DCBAA, scratchpad buffers, command ring, one interrupter.
  Intel 7/8/9-series PCH port routing (`XUSB2PR`/`USB3_PSSEN`) as Linux does.
- **Polled.** Interrupter 0 is set up with interrupts disabled. Command completions are matched by
  command-TRB pointer; transfer events are filed per (slot, endpoint) and matched by TRB pointer, so
  stale events from cancelled transfers are ignored.
- **Every wait is a deadline** on the monotonic clock (with a 1 µs-per-read fallback clock before the
  scheduler is up), never a spin count.
- **DMA safety.** Only `DmaBuf` memory — naturally aligned buddy blocks, never crossing a 64 KiB
  boundary, below 4 GiB when the controller lacks 64-bit addressing (`mm::buddy::alloc_below`) — is
  ever handed to the controller. Callers' buffers are copied through a per-endpoint bounce buffer. A
  timed-out transfer gets Stop Endpoint + Set TR Dequeue before returning; if recovery fails the
  controller is halted, so no buffer is ever freed while the controller can still reach it. Slots are
  freed only after a successful Disable Slot.
- **Bulk transfers** of up to 64 KiB per TRB and 16 chained TRBs (1 MiB) per TD; a short IN packet
  ends the transfer. Completion codes map to `Stall`, `Babble`, `Disconnected` or `Other(cc)`.
- **MMIO** is one 32-bit `ldr`/`str` per access from inline assembly on aarch64 — no writeback
  addressing forms, which HVF cannot emulate (it aborts with `Assertion isv failed`) — and 64-bit
  registers are written low half then high half.
- **Root ports only**: 100 ms debounce, then a second pass at +200 ms; USB3 ports wait for link
  training, with a warm reset if it fails. Hubs are enumerated as devices but nothing behind them is.

### Mass storage: bulk-only transport and SCSI

`drivers/usb/src/msc/` is hardware-independent and allocation-free. `Bot` frames CBW/CSW with an
incrementing tag and implements BOT 1.0 recovery (data-stage stall clears that endpoint and reads the
CSW; CSW stall retries once; anything malformed does a reset recovery). `ScsiDisk` probes LUNs with
`TEST UNIT READY` (every 250 ms, up to 10 s, tolerating UNIT ATTENTION), reads capacity with
`READ CAPACITY(10)` then `(16)` when needed, honours the write-protect bit from `MODE SENSE(6)`,
uses `READ/WRITE(10)` or `(16)` past 2^32 sectors in chunks of at most 120 KiB, and accepts 512- and
4096-byte sectors. Each command gets three attempts (with `REQUEST SENSE`), a reset recovery, and
three more; `ILLEGAL REQUEST`, `DATA PROTECT` and `Disconnected` are not retried.

[usb_storage.rs](../../drivers/src/usb_storage.rs) wraps each LUN as a `BlockDevice` named `sdX`,
flagged removable, and registers it with `blkdev`, which scans its partitions (`sda1`, `sda2`, …).
It runs in the kernel init task after `usb_hcd::init()`, so the stick is registered before init's
first `/dev/root` attempt.

[usb_storage.rs](../../drivers/src/usb_storage.rs) issues READ/WRITE commands of up to 120 KiB (the 64 KiB chunk above was raised so a write-back run is a few whole commands), and its disk lock is a sleeping lock: the poll loop spins 300 us, then parks. Every command has a 30 s budget, and every failed attempt and reset is logged.

**The poll-yield deadlock.** The first stick boot hung all four CPUs at 400 % host CPU with no
watchdog output. This kernel runs syscalls with interrupts masked and every `spin::Mutex` is an
IRQ-off spinlock; the xHCI poll hook called `yield_now()` in the middle of F2FS/VFS block I/O, while
those layers held their locks. The parked lock holder could never run again and every other CPU
spun on the lock at IF=0. The fix (`02219e2`) follows the contract virtio-gpu already had: the poll
hook yields only when interrupts are enabled (boot-time enumeration); otherwise it spins inside a
preempt-disabled I/O window that opens an interrupt window every 256 polls, so ticks and deadlines
keep running but no task switch happens. The trade-off is that a stalled stick holds its CPU for the
whole retry ladder (5 s per transfer, 30 s per cache flush, up to six attempts plus a reset), and the
stall watchdog does not fire because ticks still arrive. Surprise removal is fast because
`Disconnected` is not retried.

### Reads before writes, and the slow-stick emulation

The PC's stick is a cheap USB 2.0 drive: a small write command takes 0.5 to 1.3 s (and is bimodal — a write outside the few allocation units the controller keeps open costs an allocation-unit merge of about a second), a read about a millisecond. The writes are background work (f2fs checkpoints, the kernel log flusher); the reads are what someone waits on. The USB layer therefore schedules by class (lanes nostall and readfirst):

- A read queues **ahead of every queued write or flush** on the disk lock (`LockClass::Prio`), so it waits only for the one write command already on the wire. The old scheme helped only a read that was already waiting when a writer queued, and never for writers that may not sleep (boot 16: 2135 ms of lock wait behind two queued 1 s writes). A queued write is overtaken by at most 64 reads, then later reads queue behind it.
- A sleepable write longer than one SCSI command hands the disk to waiting reads between commands and re-queues ahead of the other writes. A write or flush that may sleep also parks (at most 50 ms per request) while reads are queued.
- Every write registers its LBA range before queueing; a read overlapping a registered write waits for it first, so a read never overtakes a write to the same blocks. `SYNCHRONIZE CACHE` stays write-class. Split writes were considered and left out: the cost is per command, not per byte.
- `KRawMutex::lock_class` queues a waiter by class (`Fifo`, `Prio` with a bypass bound, `Resume`), and callers that may not sleep are queued too and spin on their own entry.
- A disk-lock wait of 300 ms or more prints `[IOWAIT]` (at most one per second); the `[usbms]` summary every 5 s splits lock waits into reads and writes (count, average, max) and names the three tasks blocked longest ([debugging.md](../development/debugging.md#storage-stalls-hung-lockhold-iowait-and-the-five-second-summaries)).

`usbms.wryield=0` turns all of it off (one FIFO queue). The model of the stick lives in the driver: `usbms.delay_us=N` and `usbms.wdelay_us=N` add latency to every read or write command, and `usbms.au_kib=K usbms.au_open=N usbms.merge_us=M usbms.au_hit_us=H` charge M us for a write outside the N most recently written K-KiB units and H (default 4000) inside them. `usbms.delay_us=1500 usbms.au_kib=4096 usbms.au_open=2 usbms.merge_us=1000000` reproduces the PC's pattern in QEMU (worst read lock wait with the stallprobe and dbwrite benchmarks: 2000 ms to 1005 ms, at most one write ahead of any read where it was two). All the `usbms.*` knobs are in the [reference table](../servers/filesystems.md#kernel-command-line-knobs); the file-system half of the story is [Performance on slow media](../servers/filesystems.md#performance-on-slow-media).

### Keyboards and mice

`drivers/usb/src/hid/` drives **boot-protocol** HID interfaces (class 3, subclass 1, protocol 1
keyboard / 2 mouse) over xHCI interrupt-IN pipes (`xhci/intr.rs`, `0fff571`):

- Each pipe keeps eight one-packet transfers queued and is **harvested from the BSP timer tick**
  (100 Hz, IRQ context). The tick only `try_lock`s the controller — if a task is mid-transfer the
  reports wait in the controller for the next tick — drains events without any halting wait,
  recovers a halted endpoint through a small state machine spread over several ticks, sends LED
  reports as asynchronous control requests, and never allocates (event vectors are pre-sized).
- Keyboard reports are diffed into Linux key edges using Linux's `usbkbd` usage table; typematic
  repeat is generated as repeated **presses** after the console's `KDKBDREP` delay (default 250/33 ms),
  which is how PS/2 and the virtio keyboard deliver repeats and which libinput tolerates.
- Keys go to **event0** and the mouse to **event1**, the same evdev nodes as virtio-input, so the
  console, VT switching with Ctrl+Alt+Fn, and COSMIC need no change. Because libinput rejects a node
  that is both absolute and relative and the libudev shim knows only event0/event1, relative motion
  is integrated into an absolute position (one count = one framebuffer pixel, no acceleration).
- Caps/Num/Scroll LEDs follow the VT on screen, or the compositor's `EV_LED` writes while it owns
  the keyboard. Num Lock starts on (the console default).
- Only the first boot-keyboard interface of a device is used; report-protocol-only interfaces
  (QEMU's `usb-tablet`, media keys, the PC's USB microphone's HID interface) are listed as
  unsupported.

`lsusb` shows role, port and speed; init prints `lsusb --summary` above the first login prompt (and
again after the "NO GPU RENDERER" banner repaints the console) whenever USB devices exist. In QEMU,
`run-qemu.sh <arch> --usb-input[=kbd,mouse,tablet] [--no-i8042]` replaces virtio input with USB HID,
and `--no-i8042` removes q35's PS/2 controller so USB is the only keyboard.

### The "phantom input" that was not input

The first PC boot showed a dead keyboard and a console filling with digits after `login:`. The
digits were never input: they were **kernel diagnostics with their labels stripped**. Labels went out
through a UART-only path while numbers always went to the framebuffer as well, so on a machine whose
only console is the screen, lines like `[WDOG] cpu1 took no timer tick for ~12 s … pid=0 … local_ticks=0`
arrived as `1120000`. Decoding the photographed digits against the two printers that fire on a
normal boot matched every token (the watchdog's ages 2, 12, 22 … 302 s, and greetd's repeated
`[FORK]` lines). Fix (`0535f46`): a number now follows the route of the most recent label on the same
CPU. The decoded watchdog lines in turn exposed the sparse-APIC-ID bug below.

Real hardware could inject phantom input in other ways, so those were hardened in the same change
(`cfc35d7`): the i8042 driver used to read port 0x60 on every IRQ 1 without checking status or
presence. It now trusts the ACPI FADT `IAPC_BOOT_ARCH` 8042 bit (clear on the test PC: "no PS/2
controller, not probing"), otherwise detects the controller with a self-test, rechecks after the xHCI
BIOS handoff (which ends the firmware's SMM PS/2 emulation), masks IRQ 1 when absent, and accepts
only bytes with OBF set and AUX/parity/timeout clear. COM1 reading `0xFF` from an absent UART is now
"no data" instead of an endless stream of `0xFF` bytes.

## The kernel log on the stick

Debugging a machine whose only console is a monitor used to mean photographing the screen. The
stick now carries the log (`d4d2e9f`, `121b3bd`): every console byte also goes into a lock-free
2 MiB RAM ring, and `kernel/src/klog.rs` writes it to the raw log partition **on the boot disk**,
found by partition type, never by number. The partition holds four slots, so the last four boots are
kept; each slot is a 4096-byte header (magic `LEANLOG1`, boot counter, flush sequence, length, CRCs,
flags FULL/PANIC/LOST) followed by plain text, and the data is written before the header. The log is
flushed on `sync`, on `poweroff`/`reboot`, on panic, on hung-task reports (within a second), and
periodically with an **adaptive cadence**: every 2 s on fast media, but each flush schedules the next
10 times its estimated cost later (at most 30 s), and a flush estimated at 250 ms or more (the PC's
stick: 1 to 3.3 s) schedules it 50 times later (at most 5 minutes) — about 2% of the device's time
instead of 10%. After a hard hang the tail of the log can therefore be minutes old.

Two details keep the flusher from hurting the system it is logging (lane readfirst). It snapshots
under the state lock, **writes with no lock held** (in a context that may sleep, queued behind reads
like any write), and only then commits; poweroff waits, bounded, for a running flush and then writes
synchronously, and a panic does not wait and writes over it. And it does **not rewrite the header
page** on a periodic flush — an in-place write far from the data being appended is exactly the write
that costs an allocation-unit merge. Instead the header copy (magic `LEANLOGJ`, same layout) rides in
the same write command as the data, in the page right after the data end ("journal pages"); the real
header page is still written when the slot is claimed, at the first dump, on poweroff and on panic, so
an older reader still finds a valid, if older, log. Measured with the allocation-unit model on aarch64:
27 flushes down to 11 in the same run, and flush cost 1.1-2.2 s down to 0.004-1.75 s.

```sh
diskutil unmountDisk /dev/diskN                              # macOS
sudo ./scripts/read-usb-log.sh /dev/rdiskN > boot.log        # latest boot
sudo ./scripts/read-usb-log.sh /dev/rdiskN --all             # every kept boot, oldest first
./scripts/read-usb-log.sh stick.img                          # a QEMU image
```

`read-usb-log.sh` takes the newest valid journal page of the slot's boot (highest flush sequence, at
the page its own length says) and reports on stderr which boot, slot and byte count it read, plus any
flags — so a redirected run that wrote an empty file says why. Header flags say whether the slot
filled up (FULL), the ring overran (LOST) or the kernel panicked.

The log is also the diagnostics channel for everything else on this page: init copies the tail of
`greetd.log` to the console when the graphical login dies young
([login.md](../desktop/login.md#when-the-greeter-dies-young)), every firmware load logs its CRC
([drivers.md](drivers.md#firmware-loading)), and the NVIDIA bring-up writes whole lines through the
console sink (UART, log and framebuffer together).

## CPUs on real topology

`arch_cpu_id()` on x86_64 used to be the raw LAPIC ID, indexing per-CPU arrays of eight. QEMU's IDs
are dense; the PC's are 0,2,4,6, so slots 1 and 3 belonged to no CPU and the watchdog reported them
dead from boot. `smp_init` also woke every AP with a broadcast SIPI while having stacks for only
seven. Since `963c681` CPUs get **dense indices** (`MAX_CPUS` = 64) assigned as each AP actually comes
up, APs are enumerated from the ACPI MADT (Local APIC and x2APIC entries) and started one at a time
with targeted INIT/SIPI, `IA32_TSC_AUX` holds the dense index, and x2APIC mode is used when the
firmware hands over in it or an APIC ID exceeds 254. The PC logs
`[SMP] 4 CPUs online (xAPIC) cpu:apic 0:0 1:2 2:4 3:6`. aarch64 gained the same treatment from the
MADT or the device tree. Details: [kernel/scheduler.md](../kernel/scheduler.md) and
[kernel/arch-x86_64.md](../kernel/arch-x86_64.md).

## Power

`poweroff` and `reboot` work on the PC through the ACPI tables parsed at boot: the FADT's PM1a
control block and `RESET_REG`, and `\_S5_` from the DSDT. The test PC reports
`PM1a_CNT=0x1804`, `\_S5_ SLP_TYP=7`, `RESET_REG=0xCF9 value 0x06`, and powers down cleanly. Reset
falls back from `RESET_REG` to the keyboard controller, port `0xCF9`, then a triple fault. See
[kernel/arch-x86_64.md](../kernel/arch-x86_64.md).

## NVIDIA RTX 3070

### Why this matters, and the decision

COSMIC renders only on a hardware GPU (see [graphics/README.md](../graphics/README.md)); with no GPU
driver (or with `nvrm.start` off) the PC boots to the console and init prints `## NO GPU RENDERER: the graphical login (COSMIC)
was NOT started.` That is correct behaviour, not a bug. Getting the desktop and Firefox onto the PC
needs a driver for its GPU.

The plan, decided 2026-10-07 after comparing two independent designs:

| Layer | Choice | Why |
|---|---|---|
| Kernel | **NVIDIA's OpenRM** (`open-gpu-kernel-modules`, taken under its MIT licence) linked into the kernel behind an `os_*`/`nv_*` shim | The same approach Haiku's `nvidia-haiku` port proved on real cards; RM already knows how to boot GSP firmware on every recent GPU. |
| Userspace | **Upstream Mesa NVK** (with the NAK compiler) + **Zink**, unmodified | NVK is the maintained open Vulkan driver; Zink gives GL on top, as on the virtual GPUs. |
| Glue | A **nouveau-UAPI translation** in the DRM server | NVK speaks the nouveau kernel interface. The driver name must be exactly `nouveau`, version ≥ 1.3.1, and `VM_INIT` must succeed, or Mesa falls back silently. |
| Display | Reuse the GOP framebuffer, blit with a copy engine; no modesetting | The firmware already lit the display; modesetting is a large, separate project. (Implemented as stage M8, below.) |

Kill criterion: if the shim could not reach GSP `INIT_DONE` within about four weeks, fall back to
porting nouveau's MIT-licensed r535 GSP code to Rust. (`nova-core` is GPL-2-only and serves as a
reference only.) The overall estimate is four to six months.

### Stage G1: a read-only probe

`drivers/src/nvidia_probe.rs` (`b92798a`) runs before the block layer and is a silent no-op unless a
class-0x03 function from vendor `0x10de` exists. It logs `[NVPROBE]` lines for config space, BAR
sizing (decode off, everything restored), capabilities including Resizable BAR (via the ACPI MCFG
ECAM window on x86_64, because port `0xCF8` reaches only 256 bytes), `PMC_BOOT_0`/`PMC_BOOT_42`
decoded as nouveau does, a PTIMER liveness check, the VBIOS (PROM window with the ROM-shadow bit
handled as nouveau does, falling back to the expansion ROM BAR; PCIR images, BIT table, CRC), and
where the GOP framebuffer lies within the GPU's BARs. It is bounded by a 600 ms budget. Command-line
options: `nvprobe.off=1`, `nvprobe.dumpvbios=1` (base64 VBIOS in the log, 5 s budget), and two test
modes for QEMU.

On the PC it found `10de:2484` (subsystem `1043:87be`), BAR0 16 MiB at `0xF6000000`, BAR1 256 MiB at
`0xE0000000`, BAR3 32 MiB, a Gen4 x16 link idling at Gen3, chipset `0x174` GA104, and the GOP
framebuffer at `BAR1+0x0`. (Its best-effort VRAM decode printed 2 GiB for an 8 GiB card; that register
decode is not trusted.)

### Building OpenRM freestanding

`ports/openrm/` pins OpenRM **570.86.16** — the tag `nvidia-haiku` builds on, so a working reference
port uses the same code — and its GSP firmware, extracted from NVIDIA's `.run` package by `fetch.sh`
(`linux-firmware` cannot be used: RM refuses a GSP image whose version differs from its own). All four
artefacts are pinned by SHA-256; nothing NVIDIA ships is committed. `build.sh` runs NVIDIA's own
Makefile **unmodified**, swapping only the toolchain (clang with a `linux-gnu` triple, rust-lld); the
Makefile's flags already match the kernel target (code model kernel, no red zone, soft-float, no
SSE). The result, `libnvrm.a` (14.4 MB), has no compiler-runtime, TLS or floating-point imports; its
import list is exactly the 344-symbol OS interface. A licence audit of all 2,650 compiled files found
MIT plus permissive BSD-3, zlib and SciTech components and no GPL-only file.

```sh
ports/openrm/fetch.sh                      # source + firmware, sha256-verified
./scripts/build-all.sh --nvidia            # libnvrm.a + x86_64 kernel with --features nvidia
```

The kernel links it through the `nvidia-rm` crate ([drivers/nvidia](../../drivers/nvidia/src/lib.rs)),
an optional x86_64-only dependency enabled by the kernel feature `nvidia`. A generated table keeps all
208 RM entry points alive so any unresolved OS symbol fails the link. **The default kernel is
byte-for-byte identical with and without the crate in the tree** — the hook in `kernel/src/init.rs`
is deliberately one line, because inserting lines shifts the panic locations baked into the binary.

### Stage G2: the OS shim and GSP boot

Of the 344 OS callbacks, 158 are hand-written Rust, 22 live in a small C glue file, and 164 are
generated stubs that log their name and fail. Of the 146 symbols `nvidia-haiku` really implements,
141 are implemented here; everything Haiku leaves as a panic stub (libspdm, NVSwitch, power
management, NUMA, vGPU, …) stays a stub, on the reasoning that a port running NVK on real cards
evidently never reaches them. The shim stands on new generic kernel facilities (`a1a6d71`): sleeping
mutexes, rwlocks, semaphores, wait queues and completions with an in-IRQ/may-sleep context check,
kernel threads, work queues, hrtimers, a kernel virtual address allocator with `ioremap`/`vmap`, an
IDT vector allocator with MSI/MSI-X, and `request_firmware` reading files from the mounted root. See
[drivers.md](drivers.md) and [kernel/scheduler.md](../kernel/scheduler.md).

The bring-up runs the Linux `nv_pci_probe` + `nv_start_device` sequence step by step in an `[nvrm]`
kernel thread started after userspace init (so init's PID is unchanged and the root filesystem,
which holds the 63 MB GSP image at `/lib/firmware/nvidia/570.86.16/gsp_ga10x.bin`, can appear):

| # | Step | Touches the GPU |
|---|---|---|
| 0 | `rm_init_rm` (+ registry overrides) | no |
| 1–2 | PCI handle and config dump; BAR sizing; GOP framebuffer vs BAR1 | config space |
| 3–5 | event locks; `rm_is_supported_device` (maps BAR0); RM private state | MMIO reads |
| 6–7 | wait for the real root; preload and CRC the GSP image | no |
| 8–9 | enable memory decode and bus mastering; MSI + ISR, bottom half on a work queue | yes |
| 10 | `rm_init_adapter`: VBIOS/FWSEC, firmware, radix3/WPR, booter, **GSP-RM `INIT_DONE`** (polled) | yes |

A `[nvrm-wd]` watchdog heartbeats every 10 s, dumps the last 32 callbacks every 30 s, and after
`nvrm.timeout` (90 s) aborts the step: the RM thread parks at its next sleepable callback, and if it
does not, bus mastering is cleared and MSI masked. The log is flushed to the stick before every risky
step. Options:

| Option | Meaning |
|---|---|
| `nvrm.start=1` | full bring-up (default: `rm_init_rm` and detection only) |
| `nvrm.stop_after=<step>` | stop after a named step, for bisection |
| `nvrm.timeout=<s>`, `nvrm.roottimeout=<s>` | watchdog budget per step (90), wait for the root (180) |
| `nvrm.loglevel=<n>`, `nvrm.reg=Name:val,…` | RM message floor; RM registry overrides |
| `nvrm.nomsi=1` | skip MSI (`INIT_DONE` is polled, so step 10 is still reachable) |
| `nvrm.testdev=`, `nvrm.fakegpu=`, `nvrm.fakehang=` | QEMU rehearsals against the `edu` device |
| `nvrm.conhold=0`, `nvrm.constress=<n>` | stop holding the console during the GSP boot window; print n console lines inside it ([below](#the-console-hold)) |
| `nvrm.selftest=ce\|abi`, `nvrm.selftest_stop/skip/hold/loops/fbphys` | the in-kernel RM client self-test (G3) |
| `nvrm.uapi=0`, `nvrm.uapi_log=<n>`, `nvrm.uapi_timeout=<s>` | the nouveau UAPI: do not register it; log level (0 errors, 1 every ioctl, default; 2 with arguments); kill a channel with no progress for this long (10) |
| `nvrm.kms=0`, `nvrm.kms_log=0..2` | the copy-engine KMS: opt out; log level |
| `nvrm.nomcfg=1` | do not give RM the ACPI RSDP |
| `nvrm.vktest=1`, `nvrm.desktop=0\|1` | run `/bin/nvk-autotest` at boot and keep the graphical login off (`vktest=1` without `desktop=1`); `desktop=0` keeps COSMIC off. Otherwise COSMIC starts on a nouveau card0 |

The rehearsals covered what QEMU can: 41/41 callback checks against QEMU's `edu` PCI device (config
space, MMIO, MSI, DMA in both directions through `nv_alloc_pages`, locks, delays, firmware), the full
start sequence on a stand-in device, both watchdog hang modes, and the firmware loaded from a USB
stick's root. `scripts/nvidia-test-stick.sh <device> [--quick] [--cmdline "…"]` builds everything and
writes a stick with `nvprobe.dumpvbios=1 nvrm.start=1`.

### Result on the RTX 3070 (2026-10-08)

The first hardware run passed G2 on the first try:

```
NVRM: GPU 0000:01:00.0: RmInitAdapter succeeded!
[nvrm] +11035ms ==== rm_init_adapter OK in 3528 ms: GSP-RM is up (INIT_DONE) ====
[nvrm]   device name: NVIDIA GeForce RTX 3070
[nvrm] callbacks reached: 78 distinct
[nvrm] stubs hit 0, RM log lines 17, isr 2, bh 1
```

The GOP console survived GSP initialisation (the framebuffer is at BAR1's base, so RM keeps a console
mapping when BAR1 goes virtual), and the machine powered off cleanly afterwards. Findings:
RM's `clPcieReadWord()` on the upstream bridge failed (an assertion in its LTR check, non-fatal; fixed since by handing RM the ACPI RSDP, see G3), and
the USB stick answers `SYNCHRONIZE CACHE` with a CHECK CONDITION whose sense data is all zeroes,
logged repeatedly as `flush failed`.

### G3: an RM client in the kernel

`drivers/nvidia/src/client.rs` is the kernel's own RM client (the `rm_kernel_rmapi_op` entry point Linux's in-kernel clients use), with C glue in `ports/openrm/glue/lnv_rmapi.c` and a typed Rust API; every parameter block and constant the Rust side uses is checked against NVIDIA's headers at run time (161 entries, 0 mismatches). RM also gets the ACPI RSDP now, which removed the two boot-1 warnings (`PCIE config space is inaccessible`, the `clPcieReadWord()` assertion). `nvrm.selftest=abi` proves the client path in QEMU with no GPU; `nvrm.selftest=ce` is the hardware test — device, subdevice, VA space, sysmem and vidmem, channel group + channel + copy engine, doorbell submission: CE fills and copies with semaphores polled by the CPU, colour bars drawn into the GOP console's VRAM, 1000 timed round trips, reverse teardown. It **passed on the RTX 3070 on 2026-10-08**.

### Stage M5/M6: the nouveau UAPI and NVK

`drivers/nvidia/src/uapi.rs` (lane nvuapi) registers a `gpu_uapi` backend with the DRM server after `rm_init_adapter`: card0 and renderD128 answer as `nouveau` 1.4.0 with the NVIDIA PCI identity, which is what makes Mesa choose NVK (with `HAS_VMA_TILEMODE=1` Mesa 25.3's loader also picks Zink for the nouveau device by itself and NVK advertises `VK_EXT_image_drm_format_modifier`). One RM client per DRM open. `VM_INIT` creates a `FERMI_VASPACE_A`; `VM_BIND` maps each range as a fixed-address `NV50_MEMORY_VIRTUAL` (PTE kind from the range) or into a sparse range (`uapi_vm.rs` holds the bookkeeping, host-tested); `GEM_NEW` allocates contiguous snooped sysmem (mmap by physical address) or VRAM; `CHANNEL_ALLOC` creates a graphics GPFIFO channel with an error notifier and the engine classes NVK asks for over NVIF; `EXEC` writes GPFIFO entries plus a semaphore-release fence segment and rings the doorbell. Fences are read lock-free by the DRM syncobj layer, which gained timeline syncobjs (a model of Linux's fence chain), `TIMELINE_WAIT/QUERY/SIGNAL/TRANSFER` and sync files: `SYNCOBJ_HANDLE_TO_FD`/`FD_TO_HANDLE` with the sync-file flags, `DMA_BUF_IOCTL_EXPORT/IMPORT_SYNC_FILE` (per-object reservation fences), `DMA_BUF_IOCTL_SYNC` and `SYNC_IOC_MERGE/FILE_INFO` (a sync file is an eventfd signalled from the DRM tick when its fence retires), gated on a GPU UAPI backend or `drm.syncfile=1` so the virtio paths answer exactly as before. A watchdog kills channels that hang or report an RM error.

The first NVK run on the PC killed every channel that lived through one poll (`nvktest fill` got `VK_ERROR_DEVICE_LOST`, lane nvwd): `GET_WORK_SUBMIT_TOKEN` also writes the token into the channel's error context as a timestamped notification, and the watchdog treated any nonzero entry in slot 0 as an RC error. The token is now moved to slot 3 with `SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX`, as `nvidia-push` does, and RC errors are detected by slot-0 status only, as UVM does. The channel-kill, hang and Xid lines now reach the PC's monitor whole through the console sink.

`nvktest` `dlopen`s the NVK ICD and runs `info`, `fill`, `timeline`, `compute` (buffer add), `triangle` and `triangle_depth` (a PPM and a base64 32×32 copy in the log) and `timestamps`, with bounded waits. **M5** (NVK enumerates the 3070) and **M6** (the triangle: 8/8) passed on the PC. `init` runs `/bin/nvk-autotest` when the command line has `nvrm.vktest=1` (off by default; `--cmdline "nvrm.vktest=1"` on the test stick); it recaps every subtest before the `done` line, because on the PC the console is the monitor and `[INFO]` lines scroll the results away.

### Stage M8: KMS by copy-engine blits

A second backend seam, `gpu_kms`, sits next to `gpu_uapi`: it wraps a backend GEM handle in a framebuffer (`ADDFB`/`ADDFB2`), presents it and destroys it, while the synthetic connector, CRTC and plane, the atomic properties, flip events, DRM master and the console hand-off are reused unchanged. `drivers/nvidia/src/uapi_kms.rs` is an RM client of its own with a `c7b5` channel on the first async copy engine and RM's console memory (the GOP surface) mapped into its VA space, **verified at boot by a CE read-back against BAR1** before the seam is registered. `ADDFB2` dups the GEM memory and maps it once; a present waits for the producers' fences, issues one 2D copy per damage rectangle (pitch-linear, or block-linear de-tiled by the CE) and waits for the semaphore. While a KMS backend is registered the connector lists only its fixed mode and refuses others (`SETCRTC`, atomic `MODE_ID` including `TEST_ONLY`), the primary plane advertises `IN_FORMATS` = XR24/AR24 × LINEAR, `ADDFB2_MODIFIERS` is 1 and the cursor plane is hidden; flip events come from an hrtimer at a synthetic 60 Hz vblank instead of the BSP tick. Dumb buffers keep the CPU copy into the GOP surface. `nvrm.kms=0` opts out. `nvkmstest` (dumb, nouveau GEM and NVK-rendered dma-buf buffers each flipped N times with events while a bar moves, scanout pixels checked through `/dev/fb0`, an atomic modeset and flip commits, a refused non-listed mode, then `DROP_MASTER` and a check that the text console is back) passed 6/6 on the PC. If the boot-time probe fails the line now says what each side read (word 0 and the all-ones/zero counts of the BAR1 view, the copy engine's read-back, the surface address and its phys attributes) and names the two clear cases: BAR1 reading all-ones (the console cannot update either: power-cycle) and a copy engine that wrote nothing.

### OpenGL and the desktop: Zink on NVK

`gpu-env` starts COSMIC on a nouveau card0 (Zink on NVK) unless the command line has `nvrm.desktop=0`, or `nvrm.vktest=1` without `nvrm.desktop=1` (the autotest boot, whose `nvkmstest` needs DRM master). `nvgltest` runs GLES through GBM/EGL: info (`GL_RENDERER` is `zink Vulkan 1.4 … NVK GA104`, OpenGL ES 3.2), an FBO triangle, linear and tiled `gbm_bo`s rendered through EGLImages and read with `gbm_bo_map`, a **scanout** subtest allocating exactly what cosmic-comp does (`XRGB8888`, modifier `LINEAR`, `SCANOUT|RENDERING`) because the KMS advertises only linear XR24/AR24, PRIME re-import, EGL fences and native-fence sync files, a dma-buf sync file, and throughput; 8/8 on the PC, run after `nvktest` and again on card0. Boot 6 ran a COSMIC session on the 3070.

### Waiting for the card

The OpenRM bring-up takes seconds after init, and greetd used to start about 0.8 s after init, so `gpu-env` found no renderer and the graphical login was refused for good once the root got fast enough to expose the race (boot 9). The kernel now publishes `/proc/nvrm_state` (`absent`, `probing`, `ready`, `failed`; `probing` is set when `nvrm.start=1` and a GPU exists, `ready` at the end of the UAPI registration after the KMS seam, `failed` if the bring-up fails or the watchdog finds the RM thread unresponsive); `gpu-env` polls it every 250 ms (cap 90 s) while `probing`, and init restarts the graphical login once if greetd exited 78 meanwhile. `nvrm.fakestate=<secs>:<ready|failed>` rehearses it in QEMU.

### The console hold

Boots 14 and 15 lost KMS: the surface probe read `0 of 8192 words equal`, BAR1 read `0xbad0ac9d` (the RC watchdog's GPPut garbage). RM brackets the GSP boot (FWSEC/booter, WPR2, GSP-RM's FB/MMU init, BAR1 going virtual) with `os_disable_console_access`/`os_enable_console_access`; Linux takes `console_lock()` there so fbcon stops touching the framebuffer, but ours only logged. On the PC the console surface is the GOP framebuffer in VRAM behind BAR1, and every scroll re-blits all 3 MiB of it. Boots 14 and 15 printed 29 and 24 console lines (the NIC dump of the network autotest) inside the window; every boot that reached `NVIDIA KMS ready` printed at most 6 there. Nothing had been corrupted: the faster root filesystem had merely moved that dump into the window. `drivers::framebuffer::hw_hold` now holds the console while RM has it disabled: `fb_flush` keeps the dirty box and returns, `fb_dev_written` does not read the surface back, a console without a shadow drops output, and the release repaints once. `nvrm.conhold=0` turns the hold off and `nvrm.constress=<n>` prints n lines inside the window (40 ms apart) to make the hazard deterministic, for an A/B on the PC. The investigation excluded the firmware itself: every load logs its CRC (the on-stick copy and the read path matched the host's `crc32=0x28191e52` for `gsp_ga10x.bin`), and the image is re-checked when RM has consumed it ([drivers.md](drivers.md#firmware-loading)).

### What stalled, and what was done about it

The month's slowest lessons were not about the GPU. With root on a USB 2.0 stick the system froze in ways QEMU never shows, each traced to a lock held across device I/O. In order: boot 6, 61 `[WDOG]` stalls from CPUs spinning IRQs-off on the F2FS lock behind USB-stick reads (`nvstall`: a fair ticket lock whose IRQ-masked waiters open interrupt windows and service TLB shootdowns, plus read-only shared mmaps demand-paged instead of copied whole per process); boot 7, the same lock held across synchronous small writes with its waiters spinning with preemption off, so up to 3 of 4 CPUs burned while one task waited on the stick (`fsio`: a sleeping lock, bulk reads with the lock dropped, write-back batching); boot 10, a thread parked in filesystem I/O reaped by `exit_group` while it held the (now sleeping) lock, which froze every later login (`loginhang`); boot 11, an exit-time stall of tens of seconds on a machine with more than 16 GiB of RAM (`buddyhigh`, [memory.md](../kernel/memory.md#buddy-allocator)); boot 12, the filesystem lock held 93-99% of wall time once Firefox started (`f2fsperf`, `quietio`); boot 13, metadata reads under the lock queued behind 0.5 s write commands (`nostall`, `readfirst`); boots 14 to 17, the console hold above, the stick's allocation-unit write latencies (`readfirst`), then fsync cost (`fsyncbatch`: per-file and grouped fsync), a page cache for file data (`pagecache`) and Firefox's certificate churn (`ffchurn`). Each is described, with its numbers, in [Performance on slow media](../servers/filesystems.md#performance-on-slow-media), [the page cache](../kernel/memory.md#the-unified-page-cache), [Reads before writes](#reads-before-writes-and-the-slow-stick-emulation) and [Firefox's prefs](../userland/ports.md#firefox). By boot 12 YouTube played with sound on the GPU; by boot 13 the filesystem lock was held 9.9% of the time and the long stalls were gone.

## Ethernet (Realtek r8169)

`drivers/src/r8169.rs` (lane r8169) drives the RTL8111/8168 family (PCI ids `10ec:8168/8161/8136/8169/8167`; the 2.5G 8125/8126 are not supported). Probe: D3 to D0, ASPM off, memory and bus master, the first memory BAR mapped uncached, `TxConfig` XID read into a MAC version (Linux's `RTL_GIGA_MAC_VER_xx` numbering so reports compare with the reference tables), the 8168g+ MAC hand-over from the OOB firmware, chip reset, the MAC address from ERI/IDR, 256 + 256 descriptor rings with 2 KiB buffers all below 4 GiB, and MSI on x86_64. A `[r8169]` thread loads the optional PHY patch `/lib/firmware/rtl_nic/<chip>.fw` (`ports/rtl_nic/fetch.sh` stages it; the firmware parser and interpreter are host-tested in `drivers/r8169-tests`), applies per-chip MAC tables (8168b through 8168h/ep) and PHY tables (8168g/gu/h; other revisions use the generic path and the log says so), turns EEE off, autonegotiates, and then polls the PHY status every 250 ms: link lines, register dumps at 10, 30 and 120 s without link, and a TX watchdog (frames in flight with no completion for 5 s reset the chip). `drivers/src/netdev.rs` dispatches: `eth0` is virtio-net if present, else r8169; the net server restarts DHCP on link up and serves `/proc/net/nic`.

The first data path was polled at 100 Hz, which together with a 64 KiB TCP window made the PC's downloads 2 Mbit/s; lane netperf made RX interrupt-driven in the NAPI style and the net daemon kick-driven ([networking.md](../servers/networking.md#design-decisions-and-refuted-premises)).

**Interrupt protocol (lane r8169irq, needs PC confirmation).** Boot 16 had two RX stalls (watchdog chip resets at about 235 s and 245 s) with `IntrStatus=00d4` (RxDescUnavail 0x10, RxFIFOOver 0x40, TxOK, TxDescUnavail) while `IntrMask=807b` enabled 0x10 and 0x40, and zero overflow interrupts counted. The chip's MSI fires on the rising edge of `IntrStatus & IntrMask`; the old ISR masked only RxOK and acked only the bits it had read, so an enabled bit latched between the read and the ack (RxFIFOOver right after RxDescUnavail is the natural order) kept the condition asserted: no further MSI, and nothing acked those bits again (the daemon's re-arm acked RxOK only). Now, as in Linux's `rtl8169_interrupt`/`rtl8169_poll`: the ISR writes `IntrMask=0`, acks exactly the status it read and kicks the daemon; the daemon's `rx_rearm` (after draining) writes the mask, re-reads `IntrStatus` and, if an enabled event is pending, masks, acks and polls again instead of trusting the unmask edge, and acks latched bits outside the mask so none stays set. The enabled set is Linux's for 8168 parts minus TxOK (`RxOK|RxErr|TxErr|LinkChg`); RxDescUnavail/RxFIFOOver are handled by acking and draining, never by a reset (SYSErr and the overflow bits stay enabled on MAC versions <= 6). `r8169_hw::irq_service`/`irq_rearm` hold the protocol and `drivers/r8169-tests` runs it against a model of the edge-triggered interrupt logic (the old sequence wedges there, the new one survives random interleavings).

The **RX watchdog** is the last resort: no frame consumed for 2 s while RxDescUnavail/RxFIFOOver were seen or the `rx_missed` tally rises. Frames waiting in the ring mean the net daemon is not draining: `RX backlog:` is logged once and the daemon kicked, no reset. Otherwise `RX stall: ... soft recovery #N` acks the status and re-arms the mask; a stall still there a second later resets the chip (`RX stall persists ... rx reset #N`). Each line carries the ring fill, how long ago the daemon polled and re-armed, interrupts since frames stopped and a register dump; `RX resumed ...` closes an episode. A `[r8169] stats 30 s:` line (only when counters moved) reports frames, interrupts, `rx_desc_unavail`/`rx_fifo_over` seen, **`irq recheck rescued +N (total M)`** (enabled work found pending right after unmasking: the case the re-check exists for; it can also count a frame that arrived after the ring check, which the unmask edge would have caught) and the stall-recovery totals. `/proc/net/nic` has the same counters (`irq_recheck_rescued`, `rx_desc_unavail_seen`, `rx_stall_recoveries`, `resets: N (rx stall M)`). Boot 17 (no CPU/storage stalls) had no RX stall.

| Option | Meaning |
|---|---|
| `r8169.disable=1` | do not touch the NIC |
| `r8169.fw=auto\|1\|0` | PHY firmware: auto (default, load when present), 1 (wait up to 120 s for the root), 0 (skip) |
| `r8169.msi=0` | no interrupts at all (pure polling) |
| `r8169.rxirq=0` | MSI for link and errors only; RX polled (1 kHz when busy) |
| `r8169.eee=1` | keep Energy-Efficient Ethernet (default off) |
| `r8169.speed=100` | advertise 10/100 only (bad cable or switch diagnosis) |
| `r8169.debug=1` | register dumps at link events |
| `net.test=1` | init runs `/bin/net-autotest` (`netcheck`); off by default, `--cmdline "net.test=1"` on the test stick |

Not done: checksum offload, TSO, jumbo frames, INTx on x86_64 (no `_PRT`: falls back to polling), aarch64 MSI.

## Audio (Intel HD Audio)

The PC's onboard codec is driven by `drivers/src/hda.rs` behind the same kernel audio server as virtio-sound; see [audio.md](../audio.md#the-intel-hd-audio-driver). `snd.backend=auto|virtio|hda` selects the backend and `snd.test=1` runs `/bin/snd-autotest` (off by default; `--cmdline "snd.test=1"` on the test stick).

## The NVIDIA test stick

`scripts/nvidia-test-stick.sh <device> [--yes] [--gpt] [--quick] [--cmdline "args"]` (or `--image <out.img>`) builds libnvrm, the x86_64 kernel with `--features nvidia` and the root image (staging the GSP firmware, the `rtl_nic` PHY patches and the NVK tests) and writes a stick with `write-usb.sh`. Its default command line is `nvrm.start=1 f2fs.fsync=lazy`, which boots straight to the graphical login; the boot self-tests (`nvrm.vktest=1`, `net.test=1`, `snd.test=1`) are opt-in through `--cmdline`; `--cmdline` words are composed with it, a later `key=value` replacing an earlier one (the kernel also takes the last value of a repeated key), and the result is printed. `--quick` reuses the existing userland, initrd and Limine from a previous `build-all.sh`. Before writing it prints the host crc32 of every staged firmware file, to compare with the `[FW]` lines.

Lazy fsync is **on by default** here: after boot 17 showed Firefox fsyncs taking 4 to 12 s each (every one forcing a checkpoint costing seconds on the stick), `f2fs.fsync=lazy` returns at once and commits within `f2fs.lazy_s` (5) s. A power cut can lose up to about 5 s of fsynced data; the filesystem stays consistent, and `sync`, poweroff and unmount stay synchronous. `--cmdline f2fs.fsync=strict` (any value other than `lazy`) restores per-call durability.

Bisect knobs that need no rebuild (edit the stick's `limine.conf` cmdline between boots): `nvrm.conhold=0`, `nvrm.constress=<n>`, `f2fs.nostall=0`, `usbms.wryield=0`, `f2fs.fsyncbatch=0`, `pcache=0`; the full list of storage knobs is in [filesystems.md](../servers/filesystems.md#kernel-command-line-knobs). The kernel log of the last four boots is read back with `read-usb-log.sh`.

## Limitations and open work

- **No USB hub support.** Devices behind any hub — a monitor hub, a keyboard with a built-in hub,
  or the internal rate-matching hubs that put many laptops' USB2 ports behind a hub — are not
  enumerated; the summary prints a line saying so. This is the biggest functional gap on real
  hardware. Hub work exists on a branch but is paused.
- Implemented but not confirmed on any machine: 64-byte xHCI contexts, AC64=0 controllers (only
  simulated in QEMU by forcing DMA below 4 GiB), Stop Endpoint after a real NAK timeout, command-ring
  abort. The AMD, ASMedia and VIA xHCI quirks are not ported. The BIOS handoff, scratchpad buffers
  and a firmware-initialised controller — none of which QEMU models — are proven by the PC boot.
- **Firmware DMA window**: a controller left running by firmware keeps DMAing into memory the kernel
  may already have reclaimed until `usb_hcd::init()` stops it; an early quiesce is not done.
- `usb_storage::init()` blocks the kernel init task for up to ~10 s per not-ready LUN; no hot-plug
  after boot, and the block registry has no removal path (a pulled stick keeps its `sdX` entry).
- UAS-only devices are skipped (BOT only). Report-protocol HID (tablets, media keys, game pads) is
  unsupported; the mouse has no acceleration.
- DMA sync is barrier-only, correct on x86 and QEMU virt but not on non-coherent ARM boards.
- **r8169 RX stalls: fix needs PC confirmation.** Boot 16's two watchdog resets match a dead MSI (an enabled status bit left latched); lane r8169irq made the interrupt protocol Linux's (above). Confirm with `grep -E 'r8169\] (stats|RX )'` on the next PC log: no `RX stall`/`RX backlog` lines under load. An `RX backlog` line instead points at a starved net daemon, not the NIC.
- The PipeWire sink is named "Virtio Sound Output" whatever the audio backend.
- No Wi-Fi driver; r8169 has no offloads and no 2.5G parts; HDA has no jack detection or hardware volume.
- The stick is the bottleneck: writes of 0.5 to 1.3 s per command remain, and the fixes above hide them rather than remove them. F2FS has no roll-forward fsync or NAT/SIT journaling, so lazy fsync trades a data-loss window for latency ([filesystems.md](../servers/filesystems.md#limitations)).

## Code and tests

- USB: [drivers/usb/src/xhci/mod.rs](../../drivers/usb/src/xhci/mod.rs),
  [xhci/intr.rs](../../drivers/usb/src/xhci/intr.rs),
  [msc/scsi.rs](../../drivers/usb/src/msc/scsi.rs), [msc/bot.rs](../../drivers/usb/src/msc/bot.rs),
  [hid/mod.rs](../../drivers/usb/src/hid/mod.rs), [dma.rs](../../drivers/usb/src/dma.rs),
  [drivers/src/usb_hcd.rs](../../drivers/src/usb_hcd.rs),
  [usb_storage.rs](../../drivers/src/usb_storage.rs), [usb_hid.rs](../../drivers/src/usb_hid.rs)
- Block and root: [drivers/src/blkdev.rs](../../drivers/src/blkdev.rs),
  [partscan.rs](../../drivers/src/partscan.rs), [kernel/src/klog.rs](../../kernel/src/klog.rs)
- Input hardening: [arch/x86_64/src/keyboard.rs](../../arch/x86_64/src/keyboard.rs),
  [boot/src/acpi.rs](../../boot/src/acpi.rs)
- NVIDIA: [drivers/src/nvidia_probe.rs](../../drivers/src/nvidia_probe.rs),
  [drivers/nvidia/src/device.rs](../../drivers/nvidia/src/device.rs),
  [ports/openrm/build.sh](../../ports/openrm/build.sh),
  [drivers/src/firmware.rs](../../drivers/src/firmware.rs)
- r8169, HDA and the UAPI: [drivers/src/r8169.rs](../../drivers/src/r8169.rs), [netdev.rs](../../drivers/src/netdev.rs), [hda.rs](../../drivers/src/hda.rs), [drivers/nvidia/src/uapi.rs](../../drivers/nvidia/src/uapi.rs), [uapi_kms.rs](../../drivers/nvidia/src/uapi_kms.rs), [client.rs](../../drivers/nvidia/src/client.rs), [selftest.rs](../../drivers/nvidia/src/selftest.rs), [kernel/src/hungtask.rs](../../kernel/src/hungtask.rs)
- Scripts: [scripts/write-usb.sh](../../scripts/write-usb.sh), [scripts/sparse-write.py](../../scripts/sparse-write.py),
  [scripts/read-usb-log.sh](../../scripts/read-usb-log.sh),
  [scripts/nvidia-test-stick.sh](../../scripts/nvidia-test-stick.sh)
- Host tests: `cargo test --manifest-path drivers/usb/msc-tests/Cargo.toml --target <host-triple>`
  (24 BOT/SCSI cases against a fault-injecting mock device) and `drivers/partscan-tests` (14 MBR/GPT
  cases, including corrupt primary GPTs and 4Kn disks). The host triple must be given because the
  repository's cargo config forces the kernel target.
- QEMU: `run-qemu.sh <arch> --usb-stick[=gpt]` and `--usb-input`; the `regress` suite passes on a USB
  root. Investigation records: [lane-xhci](../../artifacts/notes/lane-xhci-2026-10-05.md),
  [lane-usbinteg](../../artifacts/notes/lane-usbinteg-2026-10-05.md),
  [lane-usbhid](../../artifacts/notes/lane-usbhid-2026-10-07.md),
  [lane-openrm](../../artifacts/notes/lane-openrm-2026-10-07.md),
  [lane-nvshim](../../artifacts/notes/lane-nvshim-2026-10-07.md),
  [lane-nvclient](../../artifacts/notes/lane-nvclient-2026-10-08.md),
  [lane-nvuapi](../../artifacts/notes/lane-nvuapi-2026-10-08.md),
  [lane-nvwd](../../artifacts/notes/lane-nvwd-2026-10-08.md),
  [lane-nvkms](../../artifacts/notes/lane-nvkms-2026-10-08.md),
  [lane-nvzink](../../artifacts/notes/lane-nvzink-2026-10-08.md),
  [lane-nvstall](../../artifacts/notes/lane-nvstall-2026-10-09.md),
  [lane-nvwait](../../artifacts/notes/lane-nvwait-2026-10-09.md),
  [lane-r8169](../../artifacts/notes/lane-r8169-2026-10-09.md),
  [lane-hda](../../artifacts/notes/lane-hda-2026-10-09.md),
  [lane-netperf](../../artifacts/notes/lane-netperf-2026-10-09.md),
  [lane-fsio](../../artifacts/notes/lane-fsio-2026-10-09.md),
  [lane-loginhang](../../artifacts/notes/lane-loginhang-2026-10-10.md).

## See also

- [README.md](README.md) — targets and hosts
- [drivers.md](drivers.md) — the drivers crate and its rules
- [servers/filesystems.md](../servers/filesystems.md) — block device ABI, partitions in `/dev` and `/sys`
- [input.md](../input.md) — evdev and how keys reach the console and COSMIC
- [graphics/README.md](../graphics/README.md) — why no desktop starts without a GPU renderer
