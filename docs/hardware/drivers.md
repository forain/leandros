# Drivers

The `drivers` crate and its satellites (`usb`, `nvidia-rm`, `wifi`) hold every device driver in
LeandrOS: PCI discovery, the virtio devices QEMU provides, the SD-card and USB storage paths real
machines use, interrupts, the block-device registry, firmware loading, and the graphics and sound
drivers that the DRM and PipeWire servers sit on. This page is the map, plus the rules the drivers
follow; the graphics, audio and input drivers are described in depth in their own sections.

## Where drivers run

The drivers are linked into the kernel image (the `kernel` crate depends on `drivers`, `usb`, `wifi`
and, with the `nvidia` feature on x86_64, `nvidia-rm`) and run in kernel mode. Userspace never calls
them directly: each device class is fronted by a **server** — VFS, f2fs, DRM, evdev, PipeWire, tty,
net — that accepts IPC messages from the syscall layer and calls into the driver. Several drivers also
implement the crate's small `Driver` trait (`probe`, then `handle(ipc::Message)`), which is that seam
written down. The crate's top-level comment describes the long-term microkernel intent — drivers as
isolated servers, so a crashing driver cannot take the kernel down — but today a driver fault is a
kernel fault. See [servers/README.md](../servers/README.md) for the server layer and
[kernel/README.md](../kernel/README.md) for the overall structure.

## Module map

| Module | Purpose | Detail |
|---|---|---|
| [pci.rs](../../drivers/src/pci.rs) | Bus scan (cached), config space through port `0xCF8` (x86_64) or ECAM, ECAM from ACPI MCFG on x86_64, `PciFunction` with full 4 KiB config access | [below](#pci) |
| [irq.rs](../../drivers/src/irq.rs) | `request_irq`/`free_irq`, x86_64 IDT vector allocator, MSI and MSI-X for any PCI function, aarch64 GIC lines | [below](#interrupts) |
| [virtio.rs](../../drivers/src/virtio.rs) | Shared virtio PCI capability and virtqueue structures | [below](#virtio) |
| [virtio_blk.rs](../../drivers/src/virtio_blk.rs) | Block devices, 512-byte sectors, one lock per disk, batched multi-block requests | [block devices](#block-devices) |
| [virtio_net.rs](../../drivers/src/virtio_net.rs) | Network device under smoltcp | [servers/networking.md](../servers/networking.md) |
| [virtio_keyboard.rs](../../drivers/src/virtio_keyboard.rs) | virtio-input keyboard and tablet → evdev event0/event1 | [input.md](../input.md) |
| [virtio_rng.rs](../../drivers/src/virtio_rng.rs) | Entropy for the kernel CSPRNG | [virtual-machines.md](virtual-machines.md#entropy) |
| [snd.rs](../../drivers/src/snd.rs) | virtio-snd PCM | [audio.md](../audio.md) |
| [hda.rs](../../drivers/src/hda.rs) | Intel HD Audio playback (PCI class 0403) for the audio server | [audio.md](../audio.md#the-intel-hd-audio-driver) |
| [r8169.rs](../../drivers/src/r8169.rs), `r8169_hw.rs`, [netdev.rs](../../drivers/src/netdev.rs) | Realtek RTL8111/8168 Ethernet and the NIC backend dispatch (`eth0` is virtio-net if present, else r8169) | [pc.md](pc.md#ethernet-realtek-r8169) |
| [virtio_gpu.rs](../../drivers/src/virtio_gpu.rs) | virtio-gpu: 2D, virgl, Venus, blobs, async control queue, MSI-X | [graphics/virtio-gpu.md](../graphics/virtio-gpu.md) |
| [drm_device_interface.rs](../../drivers/src/drm_device_interface.rs), [drm_driver.rs](../../drivers/src/drm_driver.rs), [kms.rs](../../drivers/src/kms.rs), `drm/` | The DRM/KMS ioctl implementation behind the DRM server | [graphics/drm-kms.md](../graphics/drm-kms.md) |
| [framebuffer.rs](../../drivers/src/framebuffer.rs), [drm_console.rs](../../drivers/src/drm_console.rs), [vector_font.rs](../../drivers/src/vector_font.rs), `console_*.rs` | The framebuffer console | [graphics/console.md](../graphics/console.md) |
| [serial.rs](../../drivers/src/serial.rs) | Serial port driver | — |
| [blockdev.rs](../../drivers/src/blockdev.rs), [blkdev.rs](../../drivers/src/blkdev.rs), [partscan.rs](../../drivers/src/partscan.rs) | `BlockDevice` trait, runtime registry, MBR/GPT scanner, `/dev/root` | [block devices](#block-devices) |
| [sdhci.rs](../../drivers/src/sdhci.rs), [blkdev_sdhci.rs](../../drivers/src/blkdev_sdhci.rs) | SD card on the Raspberry Pi (compile-time `blkdev` on `rpi5`/`raspi4b`) | [raspberry-pi-5.md](raspberry-pi-5.md#sdhci) |
| [rpi_mailbox.rs](../../drivers/src/rpi_mailbox.rs) | VideoCore mailbox property interface and framebuffer | [raspberry-pi-5.md](raspberry-pi-5.md#display-the-videocore-mailbox-framebuffer) |
| [usb_hcd.rs](../../drivers/src/usb_hcd.rs), [usb_storage.rs](../../drivers/src/usb_storage.rs), [usb_hid.rs](../../drivers/src/usb_hid.rs) | Kernel glue for the `usb` crate | [pc.md](pc.md#the-usb-stack) |
| [firmware.rs](../../drivers/src/firmware.rs) | `request_firmware`: read a file from the mounted root into a page list; logs `[FW] <path>: <n> bytes crc32=0x…` per file | [below](#firmware-loading) |
| [nvidia_probe.rs](../../drivers/src/nvidia_probe.rs) | Read-only NVIDIA GPU probe (`[NVPROBE]`) | [pc.md](pc.md#stage-g1-a-read-only-probe) |

Satellite crates:

| Crate | Path | Purpose |
|---|---|---|
| `usb` | [drivers/usb](../../drivers/usb/src/lib.rs) | xHCI host controller (`xhci/`), bulk-only mass storage + SCSI (`msc/`), boot-protocol HID (`hid/`), `DmaBuf`, the `BulkTransport` seam. `descriptor`, `device`, `endpoint`, `transfer`, `hub` and `hcd` are protocol types and an older hub/HCD model that is not wired up. |
| `nvidia-rm` | [drivers/nvidia](../../drivers/nvidia/src/lib.rs) | NVIDIA OpenRM OS shim and bring-up thread; optional, x86_64 only, feature `nvidia` |
| `wifi` | [drivers/wifi](../../drivers/wifi/src/lib.rs) | A port of Linux's `cfg80211`/`mac80211`/`nl80211` layering with a simulated driver |
| host tests | [drivers/partscan-tests](../../drivers/partscan-tests/Cargo.toml), [drivers/usb/msc-tests](../../drivers/usb/msc-tests/Cargo.toml), `drivers/r8169-tests`, `drivers/uapi-tests` | Host-side unit tests for the partition scanner, the BOT/SCSI layer, the r8169 chip table/ring/firmware interpreter, and the DRM sync-object and VM_BIND bookkeeping |

Cargo features on `drivers`: `rpi5` and `raspi4b` select the SDHCI driver as `drivers::blkdev` instead
of virtio-blk; `usb-debug` turns on verbose xHCI tracing and a boot-time bulk-transport self-test.

## PCI

`pci::scan()` walks the bus once and caches the result; drivers look devices up by vendor/device ID or
class (`find_device`, `find_all_devices`, `find_by_class`), so a driver whose hardware is absent costs
one lookup. Config space is reached through:

- **x86_64**: port `0xCF8`/`0xCFC` for the first 256 bytes, and the ECAM window from the ACPI **MCFG**
  table (`init_ecam_from_acpi`, `ecam_va()`) for extended capabilities such as Resizable BAR and the
  ones NVIDIA's RM reads. The ECAM page of a function is validated against its legacy vendor/device ID.
- **aarch64**: the ECAM base from the device tree or ACPI. On `rpi5` it is forced to zero: the
  BCM2712 PCIe controller needs link training this kernel does not perform, and scanning it produces
  asynchronous aborts that surface much later as an SError blamed on init
  ([raspberry-pi-5.md](raspberry-pi-5.md#bring-up-six-bugs-qemu-could-not-show)).

`PciFunction` offers full 4 KiB config access with typed helpers (`read8/16/32`, `write8/16/32`,
`enable_memory`, …). BAR sizing is always done with decode switched off and everything restored.

One x86 bug is worth remembering for anyone writing inline assembly here: the config-write helpers
overwrote `eax` but declared it input-only, so LLVM reused `eax` as the *address* of the next config
access; BAR sizing read back from bus 255 and reported a "16-byte BAR". Fixed by declaring
`inout("eax") address => _` (`8a64c2b`). Any earlier write-then-read sequence could have hit it.

## Interrupts

Most drivers **poll**. The virtio block, network, input, entropy and sound drivers, the SDHCI driver,
and xHCI bulk transfers all poll their queues, and the virtio ones set the PCI command register's
**Interrupt Disable** bit (`PCI_CMD_INTX_DISABLE`). The reason is concrete: a device whose ISR status
is never read holds its INTx line asserted for good, and on QEMU `virt`'s four shared PCIe lines that
would storm the GPU's completion interrupt. USB HID reports are harvested from the timer tick rather
than from an interrupter.

Drivers that do need interrupts use [irq.rs](../../drivers/src/irq.rs):

- **x86_64**: device interrupts are IDT vectors. Vectors `0x21..=0xEF` (except the reschedule IPI at
  `0x40`) enter one generic stub that sends the LAPIC EOI, enters hard-IRQ context, and dispatches.
  Drivers get a vector with `alloc_vector` (from `0x41` up) through `request_msi` or
  `MsixTable::request`; messages target the BSP with fixed delivery, edge-triggered. Legacy INTx is not
  routed on x86_64 (there is no ACPI `_PRT` parser).
- **aarch64**: interrupts are GIC INTIDs. A line can have up to four sharers (for shared PCI INTx), is
  enabled level-triggered, and is reached through the GIC dispatcher's fallback. MSI needs a GICv3
  ITS, which is not driven yet, so `request_msi` returns `Unsupported` there.
- **Handler contract**: handlers run in hard-IRQ context with interrupts masked on whatever task was
  interrupted. They must not sleep; they may take `KSpinLock`s, wake waiters, queue work items, and
  allocate from the atomic pool. A level-triggered handler must quiesce its device before returning. A
  line on which no sharer claims 1,000 deliveries in a row is masked and logged (storm guard).

The virtio-gpu control queue's MSI-X vector and the NVIDIA bottom half both use this API; the
underlying GIC dispatch table and APIC programming are in
[kernel/arch-aarch64.md](../kernel/arch-aarch64.md) and [kernel/arch-x86_64.md](../kernel/arch-x86_64.md).

## Virtio

All virtio devices are found on PCI on both architectures (QEMU puts virtio on PCI for both `q35` and
`virt`). Each driver negotiates `VERSION_1` and parses the vendor capabilities (common, notify, ISR and
device configuration) itself. The block, network and entropy drivers accept both the modern and the
transitional device ID (input and sound are modern-only devices in QEMU anyway); the
launchers request modern-only devices on aarch64 because transitional ones deadlock QEMU 10.x's block
layer ([virtual-machines.md](virtual-machines.md#why-disable-legacyon)). `virtio.rs` holds the shared
structure definitions, but the capability walk and queue setup are currently repeated in each driver
rather than factored into a common transport — a known piece of duplication.

## Block devices

The block layer has a stable seam and two compile-time shapes:

- **`BlockDevice`** ([blockdev.rs](../../drivers/src/blockdev.rs)) is the trait every disk
  implements: sector size, capacity, a name class (`vd`, `sd`), removability, sector reads and writes,
  flush. Each device has its own lock; before the registry existed, one global table lock was held
  across every request on every disk.
- **The runtime registry** ([blkdev.rs](../../drivers/src/blkdev.rs), on QEMU and x86_64) holds up to
  64 `&'static dyn BlockDevice` entries, append-only. Whole disks are named `<class><letter>` with a
  counter per class; partitions found by [partscan.rs](../../drivers/src/partscan.rs) become further
  entries named `<disk>N` (or `<disk>pN` when the disk name ends in a digit), each recording its parent
  and start. The registry lock is held only to copy an entry in or out, never across I/O. The older
  4096-byte-block façade (`read_block`, `read_blocks`, `write_block`, `flush`, `has_f2fs`) keeps its
  signatures and adds the partition offset and bounds check. virtio disks register first and are
  scanned afterwards, so `vda`/`vdb`/`vdc` stay at indices 0, 1, 2 (the boot image's ESP becomes `vda1`
  at index 3). USB LUNs register at runtime as `sdX`.
- **The partition scanner** is a pure parser over a read callback: MBR primary entries (superfloppy FAT
  boot sectors rejected, extended containers skipped), and GPT through a protective `0xEE` entry with
  the header CRC, `my_lba` and entry-array CRC all checked and a fallback to the backup header. It
  also renders PARTUUIDs in Linux's text forms.
- **On `rpi5`/`raspi4b`** `drivers::blkdev` is the SDHCI driver instead, re-exported with the same API;
  runtime registration returns `Unsupported`.

`/dev/root` selection, the `ENODEV`/`ENOTUNIQ` semantics and the boot-disk tie-break are described in
[pc.md](pc.md#choosing-the-root-devroot). How the VFS mirrors the registry into `/dev` and
`/sys/class/block` (major 254 for `vd`, 8 for `sd`, minor = ordinal × 16 + partition) is in
[servers/filesystems.md](../servers/filesystems.md).

## Firmware loading

`request_firmware(path, max_len)` ([firmware.rs](../../drivers/src/firmware.rs)) reads a whole file by
absolute path from the mounted root into a **page list** — no large contiguous allocation, so a 63 MB
GSP image becomes about 15,000 scattered 4 KiB pages that can be consumed by copying, by iterating, as
a physical page list, or through a `vmap`. The filesystem lives above this crate (`f2fs-server`
depends on `drivers`), so the reader is a backend the kernel registers at init
([kernel/src/firmware.rs](../../kernel/src/firmware.rs)): open, `pread` and close by port, with no fd
table or user memory. It works only after userspace init's `pivot_root` (`root_ready()`), must be
called from task context, and never while holding a spinlock, because the read takes the f2fs mount
lock and does block I/O.

Every load logs one line, `[FW] <path>: <n> bytes crc32=0x…` (zlib's CRC-32 over the loaded pages), added after two PC boots lost KMS and the cause had to be excluded: the GSP preload reuses the value, and `nv_put_firmware` recomputes it after RM has consumed the image, so corruption of the in-memory copy between load and use (stray DMA, a use-after-free) prints `CHANGED SINCE LOAD`. `scripts/nvidia-test-stick.sh` prints the host-side CRC of every staged file for comparison.

## Kernel facilities for drivers

The NVIDIA work needed generic facilities that any future real-hardware driver can use (`a1a6d71`):
sleeping `KMutex`, `KRwLock`, `KSemaphore`, `KWaitQueue` and `KCompletion` (in `sched::ksync`), a
per-CPU hard-IRQ nesting counter (`sched::irqctx`) with a `might_sleep` check that logs and spins
instead of sleeping in atomic context, kernel threads, work queues, hrtimers, and a kernel virtual
address allocator with `ioremap`/`vmap` and explicit cache types (`mm::kva`). They are documented with
the scheduler and memory manager: [kernel/scheduler.md](../kernel/scheduler.md),
[kernel/memory.md](../kernel/memory.md).

## Wi-Fi

`drivers/wifi` is a structural port of Linux's wireless layering — `ieee80211` frame types,
`cfg80211` configuration, `mac80211` soft-MAC, and an `nl80211`-style control interface over IPC —
with one driver, `VirtioWifi`, which is **simulated**: transmit records frame metadata in a small ring,
and scans return pre-injected fake BSS entries. The crate is compiled into the kernel but nothing in
the kernel calls it, and there is no driver for any real wireless chip (the Pi 5's SDIO Wi-Fi and the
test PC's adapters are untouched). Networking in practice is wired: virtio-net in VMs and r8169 on the PC; see
[servers/networking.md](../servers/networking.md).

## Rules every driver follows

These come from bugs, each of which cost days:

- **Bound every wait.** Use a deadline on the monotonic clock, never a bare `loop` or a spin count;
  spin counts are 1–3 ms on a real CPU and hide forever behind a fast emulator. Give each stage a
  marker so a timeout says where it happened. The SDHCI, mailbox and xHCI drivers all hung silently
  before this rule.
- **Never yield while the caller may hold an IRQ-off lock.** Syscalls run with interrupts masked and
  every `spin::Mutex` is an IRQ-off spinlock, so a driver that calls `yield_now()` in the middle of
  block I/O can park a lock holder and freeze every CPU. Poll inside a preempt-disabled window that
  opens interrupt windows for the tick instead (the virtio-gpu `SpinWindow` contract, reused by USB).
- **Keep big structures off the stack.** Kernel stacks are 128 KiB with no guard page, and Rust has no
  placement-new: `Box::new(Big::new())` builds `Big` on the stack first. Construct in place
  (`Box::new_uninit()`), and pin sizes with compile-time assertions. `scripts/check-stack-frames.py`
  fails the build above 48 KiB per frame; it would have caught both the 573 KB xHCI structure and an
  earlier 156 KB f2fs mount frame.
- **Only hand DMA-safe memory to a device.** Use `DmaBuf` (naturally aligned, zeroed, below 4 GiB when
  needed via `mm::buddy::alloc_below` — retrying `alloc` does not work, because the buddy allocator
  hands out high memory first), never stack buffers or callers' buffers, and never free a buffer while
  the device can still reach it.
- **Cache maintenance to the Point of Coherency** for non-coherent masters on ARM (`dc civac`, not
  `dc cvau`). On x86 and QEMU `virt` DMA is coherent and the sync hooks are barriers only; that is not
  true of every ARM board.
- **MMIO on aarch64 is one plain `ldr`/`str` per access** — no writeback addressing modes, which HVF
  cannot emulate (`Assertion isv failed`); `read_volatile` can compile to one.
- **Restore what you probe.** BAR sizing, ROM-shadow bits and command registers are put back exactly;
  a probe that runs on every boot must be invisible when the hardware is not there.
- **Default-off diagnostics.** Verbose tracing is behind features or gated consts, and labels and
  numbers of one line must travel the same console route (see the "phantom input" story in
  [pc.md](pc.md#the-phantom-input-that-was-not-input)).

## Limitations and open work

- No shared virtio transport; each driver repeats the capability walk.
- No interrupt-driven block I/O; a slow device holds its CPU while polling (the USB disk lock now sleeps between polls and spins only 300 us, but virtio-blk still polls). Network RX is interrupt-driven on r8169 (MSI) and kick-driven on virtio-net.
- No MSI on aarch64 (needs a GICv3 ITS); no legacy INTx routing on x86_64.
- No IOMMU support; DMA addresses are physical addresses.
- No USB hubs, UAS, or hot-plug; no Wi-Fi driver for real hardware. A Realtek wired NIC, Intel HD Audio and the NVIDIA GPU are the real-PC drivers; see [pc.md](pc.md).
- Wi-Fi is a simulated stack, not wired into the kernel.

## Code and tests

- Crate roots: [drivers/src/lib.rs](../../drivers/src/lib.rs),
  [drivers/usb/src/lib.rs](../../drivers/usb/src/lib.rs),
  [drivers/nvidia/src/lib.rs](../../drivers/nvidia/src/lib.rs),
  [drivers/wifi/src/lib.rs](../../drivers/wifi/src/lib.rs)
- Host tests: `drivers/partscan-tests` (14 cases) and `drivers/usb/msc-tests` (24 cases), run with
  `cargo test` and an explicit host `--target`.
- In-guest: `lsblk`, `lsusb`, the `regress` suite (block I/O through every filesystem test), `disktester`
  (loop devices, GPT, BLKPG); `ktest=` boot self-tests for the kernel facilities; `nvrm.testdev=` for
  the NVIDIA shim against QEMU's `edu` device. See [development/testing.md](../development/testing.md).

## See also

- [README.md](README.md) — targets and hosts
- [virtual-machines.md](virtual-machines.md) — the devices these drivers serve in QEMU
- [pc.md](pc.md), [raspberry-pi-5.md](raspberry-pi-5.md) — the real-hardware drivers in context
- [graphics/virtio-gpu.md](../graphics/virtio-gpu.md), [audio.md](../audio.md), [input.md](../input.md)
