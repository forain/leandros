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
| [hda.rs](../../drivers/src/hda.rs), `hda_capture.rs` | Intel HD Audio playback (PCI class 0403) for the audio server; microphone capture, jack sense and the display codec | [audio.md](../audio.md#the-intel-hd-audio-driver) |
| [r8169.rs](../../drivers/src/r8169.rs), `r8169_hw.rs`, [netdev.rs](../../drivers/src/netdev.rs) | Realtek RTL8111/8168 Ethernet and the NIC backend dispatch (`eth0` is virtio-net if present, else r8169) | [pc.md](pc.md#ethernet-realtek-r8169) |
| [virtio_gpu.rs](../../drivers/src/virtio_gpu.rs) | virtio-gpu: 2D, virgl, Venus, blobs, async control queue, MSI-X | [graphics/virtio-gpu.md](../graphics/virtio-gpu.md) |
| [drm_device_interface.rs](../../drivers/src/drm_device_interface.rs), [drm_driver.rs](../../drivers/src/drm_driver.rs), [kms.rs](../../drivers/src/kms.rs), `drm/` | The DRM/KMS ioctl implementation behind the DRM server | [graphics/drm-kms.md](../graphics/drm-kms.md) |
| [framebuffer.rs](../../drivers/src/framebuffer.rs), [drm_console.rs](../../drivers/src/drm_console.rs), [vector_font.rs](../../drivers/src/vector_font.rs), `console_*.rs` | The framebuffer console | [graphics/console.md](../graphics/console.md) |
| [serial.rs](../../drivers/src/serial.rs) | Serial port driver | — |
| [blockdev.rs](../../drivers/src/blockdev.rs), [blkdev.rs](../../drivers/src/blkdev.rs), [partscan.rs](../../drivers/src/partscan.rs) | `BlockDevice` trait, runtime registry, MBR/GPT scanner, `/dev/root` | [block devices](#block-devices) |
| [sdhci.rs](../../drivers/src/sdhci.rs), [blkdev_sdhci.rs](../../drivers/src/blkdev_sdhci.rs) | SD card on the Raspberry Pi (compile-time `blkdev` on `rpi5`/`raspi4b`) | [raspberry-pi-5.md](raspberry-pi-5.md#sdhci) |
| [rpi_mailbox.rs](../../drivers/src/rpi_mailbox.rs) | VideoCore mailbox property interface and framebuffer | [raspberry-pi-5.md](raspberry-pi-5.md#display-the-videocore-mailbox-framebuffer) |
| [usb_hcd.rs](../../drivers/src/usb_hcd.rs), [usb_storage.rs](../../drivers/src/usb_storage.rs), [usb_hid.rs](../../drivers/src/usb_hid.rs) | Kernel glue for the `usb` crate | [pc.md](pc.md#the-usb-stack) |
| [iwlwifi.rs](../../drivers/src/iwlwifi.rs), `iwl_*.rs`, [wifi_ht.rs](../../drivers/src/wifi_ht.rs), [wifi_frame.rs](../../drivers/src/wifi_frame.rs), [wifi_mgr.rs](../../drivers/src/wifi_mgr.rs), [wifi_sim.rs](../../drivers/src/wifi_sim.rs) | Intel Wi-Fi (the laptop's 8265) as `wifi0`, the shared 802.11 frame, WMM/HT and Block Ack code, the connection manager, and the simulated AP used in QEMU | [below](#wi-fi), [servers/netctl.md](../servers/netctl.md) |
| [netshape.rs](../../drivers/src/netshape.rs) | `netshape=` test knob: a Wi-Fi-shaped link (rate, queues, delay, loss) in front of one netif | [servers/networking.md](../servers/networking.md#netshape-link-emulator) |
| [i915/](../../drivers/src/i915/mod.rs) | Intel Gen9 GPU: execlists, i915 UAPI, KMS, external outputs, hot-plug, power wells, backlight | [graphics/drm-kms.md](../graphics/drm-kms.md#the-i915-backend) |
| [acpi_power/](../../drivers/src/acpi_power/mod.rs) (`aml.rs`, `ec.rs`, `power.rs`, `gl.rs`), [power_sysfs.rs](../../drivers/src/power_sysfs.rs) | AML interpreter, embedded controller, battery/AC/lid, `/sys/class/{power_supply,backlight}`, sleep methods | [pc.md](pc.md#acpi-power-battery-backlight-and-hotkeys) |
| [dw_i2c.rs](../../drivers/src/dw_i2c.rs), [i2c_acpi.rs](../../drivers/src/i2c_acpi.rs), [i2c_hid.rs](../../drivers/src/i2c_hid.rs), [hid_desc.rs](../../drivers/src/hid_desc.rs), [hid_ptp.rs](../../drivers/src/hid_ptp.rs) | DesignWare I2C master, ACPI I2C enumeration, I2C-HID and Precision Touchpad | [pc.md](pc.md#touchpad-i2c-hid-precision-touchpad) |
| [pci_pm.rs](../../drivers/src/pci_pm.rs), [lapinv.rs](../../drivers/src/lapinv.rs) | PCI config-space save/restore for suspend; read-only laptop device inventory | [pc.md](pc.md#suspend) |
| `usb/src/uvc.rs`, `kernel/src/v4l2.rs` | UVC webcam over isochronous xHCI pipes and the V4L2 `/dev/video0` surface | [pc.md](pc.md#webcam) |
| [firmware.rs](../../drivers/src/firmware.rs) | `request_firmware`: read a file from the mounted root into a page list; logs `[FW] <path>: <n> bytes crc32=0x…` per file | [below](#firmware-loading) |
| [nvidia_probe.rs](../../drivers/src/nvidia_probe.rs) | Read-only NVIDIA GPU probe (`[NVPROBE]`) | [pc.md](pc.md#stage-g1-a-read-only-probe) |

Satellite crates:

| Crate | Path | Purpose |
|---|---|---|
| `usb` | [drivers/usb](../../drivers/usb/src/lib.rs) | xHCI host controller (`xhci/`), bulk-only mass storage + SCSI (`msc/`), boot-protocol HID (`hid/`), `DmaBuf`, the `BulkTransport` seam. `descriptor`, `device`, `endpoint`, `transfer`, `hub` and `hcd` are protocol types and an older hub/HCD model that is not wired up. |
| `nvidia-rm` | [drivers/nvidia](../../drivers/nvidia/src/lib.rs) | NVIDIA OpenRM OS shim and bring-up thread; optional, x86_64 only, feature `nvidia` |
| `wifi` | [drivers/wifi](../../drivers/wifi/src/lib.rs) | A port of Linux's `cfg80211`/`mac80211`/`nl80211` layering with a simulated driver |
| [ena.rs](../../drivers/src/ena.rs), [ena_hw.rs](../../drivers/src/ena_hw.rs) | Amazon ENA (AWS EC2 Nitro NIC, PCI 1d0f:ec20): polled admin queue + AENQ, readless MMIO (PCI revision bit 0), one host-placement TX/RX queue pair, keep-alive/fatal/admin-timeout watchdog with reset + re-init, registered as `ethN`; `ena.disable=1` skips the probe. Core is host-tested against a mock device in `drivers/ena-tests`; LLQ and MSI-X are separate work, not tested on real EC2 yet | [design-ec2](../../TODO.md) |
| host tests | [drivers/partscan-tests](../../drivers/partscan-tests/Cargo.toml), [drivers/usb/msc-tests](../../drivers/usb/msc-tests/Cargo.toml), `drivers/r8169-tests`, `drivers/ena-tests` (mock ENA device), `drivers/uapi-tests` | Host-side unit tests for the partition scanner, the BOT/SCSI layer, the r8169 chip table/ring/firmware interpreter, and the DRM sync-object and VM_BIND bookkeeping |

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
  enabled level-triggered, and is reached through the GIC dispatcher's fallback. MSI/MSI-X go through the GICv3
  **ITS** (lane a64its): the kernel builds the command queue and device/collection tables, maps the
  function's requester ID to a DeviceID through the ACPI IORT, and delivers each vector as an LPI.
  virtio-net and NVMe (`nvme.msix=1`) use it, `/proc/interrupts` lists the LPI counts, and
  `gic.its=0` turns it off. Limits: a flat device table (DeviceID below 65536), delivery to the BSP
  only, IORT SMMU chains are not followed, and HVF refuses an ITS so the Mac stays on INTx. Real
  Graviton is unverified. Without an ITS `request_msi` returns `Unsupported` and drivers poll.
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

- **virtio-blk completion wait** is hybrid: the waiter polls the used ring for `virtio_blk.spin_us`
  microseconds (default 50) and only then parks (`wfi`/`hlt`) until the completion interrupt (MSI-X on
  x86_64, the shared INTx SPI on aarch64). Parking straight away was measured slower on aarch64 HVF
  (wake latency of about 0.35 ms per request). Page-fault and IRQs-masked callers open their own
  interrupt window. After 64 consecutive parked waits with no interrupt (line masked by the storm
  guard, broken routing) the driver goes back to pure polling and logs it.
  On aarch64 an INTx line has at most 4 sharers; virtio-blk registers one handler per INTID
  (shared by all its disks), so it costs one slot per line, and a refused registration leaves that
  disk polling (`line registration refused` in the boot log).

  | Knob | Effect |
  |---|---|
  | `virtio_blk.irq=0` | never park: spin on the used ring (pre-IRQ behaviour) |
  | `virtio_blk.spin_us=N` | spin window before parking, default 50 |

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
and scans return pre-injected fake BSS entries. It is the test driver; the real driver, `iwlwifi`, follows
and is a loadable module on the kernel-owned Wi-Fi radio class (`kmodwifi`, see [kernel/modules.md](../kernel/modules.md)).

The real Wi-Fi driver is `iwlwifi` in the `drivers` crate: Intel's 8265 (the laptop) as `wifi0`, with
WPA2, scanning, roaming and the connection manager that `/dev/netctl` and the NetworkManager shim drive
([servers/netctl.md](../servers/netctl.md)). In QEMU, `wifi_sim` stands in for the radio and an access
point, running the same station-side code paths. WPA3-Personal (SAE with P-256, hunt-and-peck and hash-to-element, anti-clogging, a PMKSA cache), 802.11w protected management frames (BIP-CMAC, SA Query), WPA3 transition fallback and roaming are implemented in the connection manager and run against `wifi_sim`'s SAE/MFP access points; the real radios (iwlwifi and the Pi 5's brcmf) do not yet report the `SAE`/`PMF`/`ROAM` capabilities, because firmware key install for BIP/IGTK and SAE authentication frames are not done, so WPA3 is unverified against a real AP. On 2.4 GHz the 8265 may use HT40 with the 20/40 coexistence rules (the AP must offer it and no 40 MHz-intolerant BSS may be seen in the span; `iwlwifi.ht40_24=0` disables it).

### Wi-Fi: QoS, HT and Block Ack

Until lane wifiht (2026-10-17) the 8265 associated as a legacy 802.11b/g station, without QoS or HT, and
downloads ran at about 1.6 Mbit/s with the AP retrying 27 % of its frames. It now associates with WMM and
HT (20 MHz, or 40 MHz on 5 GHz), accepts the AP's receive Block Ack sessions and aggregates its own
transmissions:

- **Association.** QoS and HT are chosen from the beacon (HT needs WMM, the AP's HT capabilities and
  operation elements, and an HT-capable SKU, as in mac80211). The association request carries our HT
  capabilities — what Linux advertises for the 8265, restricted to 20 MHz: LDPC, SGI20, RX STBC, MCS
  0–15, A-MPDU 64 KiB — and the WMM information element. What the AP grants (read from the association
  response) sets the station's HT flags, the AP's EDCA parameters and HT protection in the firmware.
  Data then goes out as QoS Data on TID 0 with driver-assigned sequence numbers. Beacons are watched for
  HT operation-mode changes.
- **Rate control** uses one ladder ordered by expected throughput: the legacy rates below MCS 0, then
  MCS 0–7, then optionally MIMO2 MCS above MCS 7. The starting MCS comes from the signal strength.
- **RX Block Ack.** An ADDBA request from the AP is answered after setting up a host reorder session and
  the firmware's BA session. The 8265 uses the older RX path without a firmware reorder buffer, so — as
  in Linux, where mac80211 reorders for this device — the host reorders: a per-TID buffer with
  mac80211's rules (100 ms hole timeout, window moves, BAR handling). Duplicate detection and the CCMP
  replay check run *after* reordering (mac80211's order), since retransmitted A-MPDU subframes carry
  lower packet numbers. A DELBA flushes the buffer.
- **TX aggregation** (lane wifi2, `drivers/src/iwl_agg.rs`), as iwlmvm does it on this family: the BSS
  data queue is configured as an aggregation queue for TID 0 at association, and the driver numbers QoS
  data so that each frame's sequence number equals its ring index (the scheduler parses Block Acks by
  index). Once the link is up and the queue is empty, we send an ADDBA request (window 64). On success,
  ADD_STA clears the TID in `tid_disable_tx` and the LQ frame limit becomes min(63, the AP's window); the
  firmware then builds A-MPDUs itself. TX responses with more than one frame only carry the rate; the
  BA notification reclaims the ring and gives rate control the sent/acked counts. A frame that fails
  alone gets a BAR so the AP's reorder buffer moves on. A DELBA from the AP ends the session; a missing
  ADDBA response is retried twice, 10 s apart.
- **40 MHz** on 5 GHz only, when the AP's HT operation element names a secondary channel and lets
  stations use it, the AP advertises 20/40, and the NVM allows 40 MHz on both halves. Then the PHY
  context runs 40 MHz (control position from the secondary offset), our HT capabilities advertise 20/40
  and SGI40, the station gets `STA_FLG_FAT_EN_40MHZ`, and rate control uses 40 MHz MCS with rs.c's
  40 MHz tables. 2.4 GHz stays at 20 MHz (no 20/40 coexistence scanning). The boot log line
  `BSS width: …` says why a 5 GHz BSS got 20 or 40 MHz.
- **WMM parameter changes** in later beacons (a new parameter set count, as mac80211 checks) update the
  MAC context's EDCA.
- **Firmware statistics.** The 8265's firmware sends STATISTICS_NOTIFICATION after beacons in the v13
  layout (564 bytes, API flag NEW_RX_STATS); the driver keeps the beacon count, average and per-chain
  beacon energy, the raw per-chain beacon-silence values (not dBm), channel load and ACK/CTS/BA timeouts.
- **Rate control** also compares the two antennas while the link is healthy: every 30 s, or sooner when
  the other chain hears the AP's beacons 6 dB stronger, it moves to the other antenna at the same rate
  for at least 1 s and a valid window, and keeps it only if its throughput is 25 % better. A ladder
  that went stale while idle restarts from the rate the current beacon signal suggests. With TX
  aggregation it uses rs.c's A-MPDU throughput columns.
- **Band preference.** Autoconnect (`wifi_pref.rs`) prefers a usable 5 GHz BSS (at least −80 dBm) to
  2.4 GHz ones of the same priority, unless a network was already connected this boot (then the most
  recent one wins, as before). On the laptop, 'Anvara 5G' at −76 dBm gave 5× the throughput of 'Anvara'
  on 2.4 GHz at −73 dBm, with a third of the AP retries (artifacts/notes/lane-wifi2-2026-10-18.md).
  `wifi.prefer5=0` turns it off.

Kernel command-line knobs (`[iwlwifi] QoS/HT: …` in the boot log shows which are active):

| Knob | Default | Effect |
|---|---|---|
| `iwlwifi.ht=0` / `wmm` / `1` | `1` | legacy association exactly as before / WMM only / WMM + HT20 + RX Block Ack |
| `iwlwifi.ba=0` | on | decline ADDBA: HT rates without A-MPDU |
| `iwlwifi.htrate=0` | on | HT association, legacy TX rates |
| `iwlwifi.txsgi=0\|1` | unset | short guard interval: unset lets the rate-control column search try SGI when the AP receives it (HT only; VHT 80 stays long GI), `1` starts on SGI, `0` never uses it |
| `iwlwifi.txmimo=1` | off | TX MIMO2 MCS 12–15 (off because Bluetooth coexistence shares an antenna) |
| `iwlwifi.smps=static\|dynamic` | off | the SM power save we advertise; `static` makes the AP send one stream |
| `iwlwifi.ldpc=0` | on | do not advertise LDPC receive |
| `iwlwifi.ht40_24=0` | on | do not use 40 MHz on 2.4 GHz; by default 20/40 is used only when the AP offers it and no 40 MHz-intolerant BSS is seen in the span |
| `iwlwifi.txagg=0` | on | no TX aggregation (no ADDBA request from us) |
| `iwlwifi.ht40=0` | on | 20 MHz only, also on 5 GHz |
| `iwlwifi.txant=a\|b` | automatic | transmit on one antenna only: no comparison, no toggling |
| `iwlwifi.btcoex=wifi` | off | BT_CONFIG mode BT_COEX_WIFI (measured: no gain on the laptop) |
| `wifi.prefer5=0` | on | autoconnect without the 5 GHz preference |

`/proc/net/nic` (the `wifi0` section) reports it, and a `wlan` summary is logged every 60 s:

- `ht:` — the knob, whether QoS/HT are in use, the AP's streams, SM PS and A-MPDU parameters.
- `rx_ba:` — sessions and the reorder counters (`direct`, `buffered`, `released`, `timeouts`, `bars`, …).
  `buffered` > 0 means A-MPDU holes are being closed; many `timeouts` mean subframes are lost for good,
  each costing a 100 ms stall.
- `rx_quality:` — data frames the AP sent us, how many were retries, its rate histogram (`MCSn` or
  legacy rates), the RX ring high-water mark and the longest gap between ring services. A high retry
  rate points at the radio channel; mostly 1–11 Mb/s rates mean the AP gave up on OFDM for us.
- `tx_rate:` — our rate control's current rate and per-rate success, A-MPDUs counted, antenna probes
  (`probes`, `kept`) and restarts after idle (`reseeds`).
- `tx_ba:` — the TX Block Ack session (`on winN`, `requested`, `off`), ADDBA counters, A-MPDU responses
  (count, frames, average and largest), BA notifications (`txed`/`acked`), single-frame failures and
  BARs sent, and `misaligned` (sequence number ≠ ring index; must stay 0).
- `fw_stats:` — the firmware statistics above plus our per-chain beacon energy EWMA.
- `rx_quality:` also counts the AP's 40 MHz frames (`40 MHz frames N`).

`scripts/wifibench.py`, run from a wired machine against `LEANDROS_REMOTE`, measures the real link:
N MB each way over SSH, ping, and the deltas of the AP's retry share and our retries per frame.

In QEMU every simulated AP is an HT AP and sends A-MPDU bursts with held, duplicated or lost
subframes, so every `wifi_sim` boot in `regress` and `net-e2e` exercises the reorder path. Its knobs:
`wifisim.ht=0`, `wifisim.ba=0`, `wifisim.reorder=0`, `wifisim.ampdu=<hold>:<dup>:<loss>` (per mille,
default `30:10:0`). The `wifisimtest` self-test includes `ht-addba`, `ht-reorder` and
`ht-reorder-loss`; `wifihtbench.py` measures host → guest throughput over `wifi0`.

## Loadable drivers

Some drivers no longer live only in the kernel image: e1000e, HDA (and virtio-snd), iwlwifi, UVC, USB HID, USB storage, NVMe (with virtio-blk), virtio-net, USB Bluetooth and virtio-rng build as modules with Y/M/N selection and are bound through kernel-API (`kapi_*`) shims. How modules are built, aliased, autoloaded and unloaded is in [kernel/modules.md](../kernel/modules.md). The module map above still names the source file; the load path is the thing that changed.

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
- virtio-blk waits are hybrid (spin, then park on the completion interrupt); a slow USB or NVMe device still holds its CPU while polling (the USB disk lock sleeps between polls and spins only 300 us). Network RX is interrupt-driven on r8169 (MSI) and kick-driven on virtio-net.
- No MSI on aarch64 (needs a GICv3 ITS); no legacy INTx routing on x86_64.
- No IOMMU support; DMA addresses are physical addresses.
- No UAS (bulk-only transport only). USB hubs and mass-storage/HID hot-plug work; xHCI endpoint recovery disables only the failing slot (`quarantine_slot`), and a controller that disappears (Thunderbolt power-down) is guarded by `usb.tbxhci`. See [pc.md](pc.md#usb-c-and-thunderbolt).
- Real-hardware drivers: a Realtek wired NIC, Intel e1000e, Intel HD Audio, Intel iwlwifi Wi-Fi and Bluetooth, the NVIDIA GPU (desktop PC) and the Intel i915 GPU (laptop); see [pc.md](pc.md).
- No interrupt on the I2C touchpad (GPIO not wired; it is polled), no HDMI/DP audio playback, no Thunderbolt driver, and S3 on the laptop is unresolved ([pc.md](pc.md#suspend)).

## Code and tests

- Crate roots: [drivers/src/lib.rs](../../drivers/src/lib.rs),
  [drivers/usb/src/lib.rs](../../drivers/usb/src/lib.rs),
  [drivers/nvidia/src/lib.rs](../../drivers/nvidia/src/lib.rs),
  [drivers/wifi/src/lib.rs](../../drivers/wifi/src/lib.rs)
- Host tests: `drivers/partscan-tests` (14 cases), `drivers/usb/msc-tests` (24 cases) and
  `drivers/iwlwifi-tests` (115 cases: firmware command encodings, rate control, WMM/HT elements, ADDBA,
  the reorder buffer), run with `cargo test` and an explicit host `--target`.
- In-guest: `lsblk`, `lsusb`, the `regress` suite (block I/O through every filesystem test), `disktester`
  (loop devices, GPT, BLKPG); `ktest=` boot self-tests for the kernel facilities; `nvrm.testdev=` for
  the NVIDIA shim against QEMU's `edu` device. See [development/testing.md](../development/testing.md).

## See also

- [README.md](README.md) — targets and hosts
- [virtual-machines.md](virtual-machines.md) — the devices these drivers serve in QEMU
- [pc.md](pc.md), [raspberry-pi-5.md](raspberry-pi-5.md) — the real-hardware drivers in context
- [graphics/virtio-gpu.md](../graphics/virtio-gpu.md), [audio.md](../audio.md), [input.md](../input.md)
