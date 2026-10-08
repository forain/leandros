# Real PCs (x86_64 UEFI)

A real x86_64 PC boots LeandrOS from a USB stick: UEFI firmware loads Limine from the stick, the
kernel finds its F2FS root on the same stick through its own xHCI, bulk-only-transport and SCSI
drivers, and a USB keyboard logs in at the console. On an NVIDIA RTX 3070, OpenRM — NVIDIA's open
resource manager, linked into the kernel — boots the GPU's GSP firmware to `INIT_DONE`. Rendering on
that GPU is the next stage; until it lands there is no desktop on the PC, by policy.

## Status (2026-10-08)

| Capability | State | Since |
|---|---|---|
| UEFI boot from a USB stick, root F2FS on the stick | **Works on real hardware** | QEMU `8177771` (2026-10-05); PC 2026-10-07 |
| USB HID boot keyboard and mouse | **Works on real hardware** (low-speed Logitech keyboard and mouse) | `0fff571`; PC 2026-10-08 |
| Kernel log persisted on the stick | Works | `d4d2e9f` |
| All CPUs online on sparse APIC IDs | Works (4 CPUs, APIC IDs 0,2,4,6) | `963c681` |
| `poweroff` through ACPI S5 | Works | — |
| NVIDIA RTX 3070: GSP-RM `INIT_DONE` | **First try**, 3.5 s, zero unimplemented callbacks reached | `95bd575`; PC 2026-10-08 |
| NVIDIA rendering (Vulkan/GL), desktop | Not yet | — |
| USB hubs | Not supported (work started, paused) | — |
| Ethernet, Wi-Fi, audio | No drivers for real devices | — |

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
uses `READ/WRITE(10)` or `(16)` past 2^32 sectors in chunks of at most 64 KiB, and accepts 512- and
4096-byte sectors. Each command gets three attempts (with `REQUEST SENSE`), a reset recovery, and
three more; `ILLEGAL REQUEST`, `DATA PROTECT` and `Disconnected` are not retried.

[usb_storage.rs](../../drivers/src/usb_storage.rs) wraps each LUN as a `BlockDevice` named `sdX`,
flagged removable, and registers it with `blkdev`, which scans its partitions (`sda1`, `sda2`, …).
It runs in the kernel init task after `usb_hcd::init()`, so the stick is registered before init's
first `/dev/root` attempt.

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
flushed every 2 s, on `sync`, on `poweroff`/`reboot`, and on panic.

```sh
diskutil unmountDisk /dev/diskN                              # macOS
sudo ./scripts/read-usb-log.sh /dev/rdiskN > boot.log        # latest boot
sudo ./scripts/read-usb-log.sh /dev/rdiskN --all             # every kept boot, oldest first
./scripts/read-usb-log.sh stick.img                          # a QEMU image
```

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
driver the PC boots to the console and init prints `## NO GPU RENDERER: the graphical login (COSMIC)
was NOT started.` That is correct behaviour, not a bug. Getting the desktop and Firefox onto the PC
needs a driver for its GPU.

The plan, decided 2026-10-07 after comparing two independent designs:

| Layer | Choice | Why |
|---|---|---|
| Kernel | **NVIDIA's OpenRM** (`open-gpu-kernel-modules`, taken under its MIT licence) linked into the kernel behind an `os_*`/`nv_*` shim | The same approach Haiku's `nvidia-haiku` port proved on real cards; RM already knows how to boot GSP firmware on every recent GPU. |
| Userspace | **Upstream Mesa NVK** (with the NAK compiler) + **Zink**, unmodified | NVK is the maintained open Vulkan driver; Zink gives GL on top, as on the virtual GPUs. |
| Glue | A **nouveau-UAPI translation** in the DRM server | NVK speaks the nouveau kernel interface. The driver name must be exactly `nouveau`, version ≥ 1.3.1, and `VM_INIT` must succeed, or Mesa falls back silently. |
| Display | Reuse the GOP framebuffer, blit with a copy engine; no modesetting | The firmware already lit the display; modesetting is a large, separate project. |

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
mapping when BAR1 goes virtual), and the machine powered off cleanly afterwards. Open findings:
RM's `clPcieReadWord()` on the upstream bridge fails (an assertion in its LTR check, non-fatal), and
the USB stick answers `SYNCHRONIZE CACHE` with a CHECK CONDITION whose sense data is all zeroes,
logged repeatedly as `flush failed`.

### What comes next

- **G3**: an in-kernel RM client and a copy-engine self-test (`nvrm.selftest=ce`), the first real
  work submitted to the GPU and the basis of the framebuffer blit.
- The `rm_ioctl` path through the DRM server with the nouveau-UAPI translation, user mappings and
  events, so that NVK can open the device; then Zink on NVK for COSMIC. Mesa's NVK and NAK already
  build in the GPU stack (`a92d74b`) and are staged into the image when present, but nothing selects
  them yet. See [graphics/mesa.md](../graphics/mesa.md).

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
- No NIC, audio or Wi-Fi driver for real devices; no GPU rendering, so no desktop.

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
- Scripts: [scripts/write-usb.sh](../../scripts/write-usb.sh),
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
  [lane-nvshim](../../artifacts/notes/lane-nvshim-2026-10-07.md).

## See also

- [README.md](README.md) — targets and hosts
- [drivers.md](drivers.md) — the drivers crate and its rules
- [servers/filesystems.md](../servers/filesystems.md) — block device ABI, partitions in `/dev` and `/sys`
- [input.md](../input.md) — evdev and how keys reach the console and COSMIC
- [graphics/README.md](../graphics/README.md) — why no desktop starts without a GPU renderer
