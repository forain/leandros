# Raspberry Pi 5

LeandrOS boots on a real Raspberry Pi 5 (BCM2712, Cortex-A76) from its SD card: firmware → kernel →
init → F2FS root on the card → `pivot_root` → getty → `login:` → an interactive `brush` shell, all
over the board's debug UART, and since late October 2026 also over the network (Ethernet, Wi-Fi, USB),
with Bluetooth, HDMI audio and a V3D-rendered GL stack. This page covers the `rpi5` build, preparing and
updating a card, the bring-up bugs that only real silicon exposed, the RP1 / Wi-Fi / GPU / board drivers,
and the network kernel-update flow. Bluetooth has its own page: [servers/bluetooth.md](../servers/bluetooth.md).

## Status

| Area | State |
|---|---|
| Boot to a shell prompt over serial | **Working** since 2026-08-20 (`bc9de03`..`b328db7`) |
| SD card (SDHCI) read/write, F2FS root | **Working**; PIO, single-sector by default (see [SDHCI](#sdhci)) |
| PCIe (`pcie2`) and RP1 | **On by default** since 2026-10-28; `pcie.rpi5=0` opts out |
| Ethernet (RP1 Cadence GEM + BCM54213PE, `eth0`) | **Working** (1000M up in the 2026-10-28 matrix); bulk send fixed by CUBIC, see [Ethernet](#ethernet-gem--macb) |
| USB (RP1 DWC3, two xHCI roots) | Probed and enumerated on hardware (`lsusb` shows two 1d6b:0003 root hubs) |
| Wi-Fi (CYW43455 over SDIO, `wifi0`) | **Working**: firmware download, WPA2 join with a host 4-way handshake, DHCP; survives kexec |
| Bluetooth | `/dev/hci0` over uarta, `btd`/`bthid` supervised by init; see [servers/bluetooth.md](../servers/bluetooth.md) |
| Network kernel updates | **Working**: `remote.py pi-deploy` / `pi-boot` / `pi-promote` / `pi-sync`; serial push as the no-network fallback |
| V3D 7.1 GPU | Ladder green on hardware (`v3d.probe=1`); Mesa `v3d` renders GLES; no display path for scanout apps yet |
| HDMI audio | Implemented (IEC958, MAI, dma40 ring) as a PipeWire backend; audible output not confirmed in the lane notes |
| Watchdog, panic reset, fan, RNG, RTC, thermal | Implemented; fan curve and wedge/panic behaviour confirmed on the Pi where noted below |
| HDMI console via the VideoCore mailbox | Implemented and proven on QEMU `raspi4b` (`f480fbe`) |
| SMP | Four cores via PSCI; kexec keeps all four (secondaries are parked, not powered off) |

## Building

Build with the `rpi5` feature to select the correct UART, GIC, and MMU addresses for the BCM2712 SoC,
and to link the direct-boot kernel at the address the Pi firmware loads it to:

```sh
./scripts/build-all.sh --arch aarch64 --rpi5
```

`build-all.sh` derives the Pi's linker script from `linkers/aarch64-direct.ld` with a single `sed`
that changes `KERNEL_PHYS` from QEMU virt's `0x40080000` to **`0x00200000`** (written to
`target/aarch64-direct-rpi5.ld`), so the two cannot drift apart. It then produces the flat image
`target/final-aarch64/kernel-direct.bin` with `objcopy -O binary`.

> An older revision of the README said the Pi 5 kernel links at `0x80000`. That was the generic
> AArch64 default and is not what the tree does: the firmware's own `kernel_2712.img` loads at
> `0x200000`, and `0x40080000` is refused outright (see bug 2 below). The comments in
> `scripts/deploy-rpi5.sh` still mention `0x80000` and are stale.

`build-all.sh` looks for `llvm-objcopy` **and** `rust-objcopy` (in the toolchain sysroot and on
`PATH`). It used to probe only `llvm-objcopy`; on a toolchain that ships `rust-objcopy` the flat image
silently stopped being regenerated and was **2.5 weeks stale** before anyone noticed (`c1d4162`),
because the SD-card scripts only test that the file *exists*. A missing objcopy now prints a loud
warning instead.

**Image provenance pitfall.** `--rpi5` (and `--raspi4b`) overwrite the shared
`leandros-limine-aarch64.img` with a board-feature kernel. Booting that image on QEMU `virt` faults
in early console setup — on the BCM UART address — *before printing anything at all*. When a
QEMU aarch64 boot is silent, check which feature set the image was built with before debugging
code; rebuild without `--rpi5` to get a genuine virt kernel.

## Preparing an SD card

`scripts/prepare-rpi5-sdcard.sh` wipes a card, creates an MBR with a **512 MiB FAT32 boot
partition** (`RPI5BOOT`) plus a **type-0x83 F2FS root partition** filling the rest, writes the
populated F2FS root built by `scripts/mkfs-f2fs-populated.py` (the same userland as the QEMU disks),
and populates the boot partition for one of two boot paths. Linux and macOS hosts are both
supported.

```sh
# RPi firmware loads the flat kernel image directly, no bootloader in between
sudo ./scripts/prepare-rpi5-sdcard.sh --boot-mode direct /dev/mmcblk0   # or /dev/diskN on macOS

# RPi firmware loads the vendored RPI_EFI.fd, which boots Limine, which loads kernel.elf + initrd
sudo ./scripts/prepare-rpi5-sdcard.sh --boot-mode limine /dev/mmcblk0
```

It downloads and caches the Broadcom GPU firmware blobs (`start4.elf`, `fixup4.dat`) from the
[RPi firmware repo](https://github.com/raspberrypi/firmware/tree/master/boot) on first run; pass
`--firmware-dir <dir>` to supply them from a local copy instead. The Limine path uses the same
Limine release as `build-all.sh` (11.4.1, protocol revision ≥ 6).

macOS has no "Linux" partition type in `diskutil`, so partition 2 is created as a placeholder and its
MBR type byte is patched to `0x83` afterwards. The kernel's SDHCI driver finds the root by parsing the
MBR for the first `0x83` entry (`find_f2fs_partition` in `drivers/src/sdhci.rs`); the USB-stick
writer uses the same scheme, so both media are found the same way.

**Device safety.** The script rejects nonexistent disks, partitions and the root disk, and asks you
to type the device name — unless `-y` is passed, which skips that confirmation. Device numbers move:
the same card has enumerated as different `/dev/diskN` nodes on the same Mac across sessions. Always
re-derive the node (`diskutil list external`, or `diskutil info` showing Protocol USB, Location
External, Removable Yes) immediately before each run, and never reuse a device path from shell
history. A dependable external USB SD reader is the recommended way to write cards from a laptop.

### What the direct-boot `config.txt` says, and why

| Setting | Why |
|---|---|
| `kernel=kernel.img`, `arm_64bit=1` | The firmware does not parse ELF for `kernel=`: it copies the file verbatim and branches to its first byte, so the card carries the flat image. |
| `kernel_address=0x200000` | The VideoCore does the loading and reaches only low memory. `0x40080000` is refused *before the file is read*. |
| `initramfs initrd.cpio 0x10000000` | Same low-memory rule; must match `INITRD_PHYS` in `kernel/src/main.rs` (QEMU uses `0x48000000`). Above the kernel image, below the firmware's own framebuffer at `0x3f800000`. |
| `enable_uart=1` | Serial console on the debug connector. |
| `uart_2ndstage=1` | Makes the bootloader narrate every load — the diagnostic for refused loads. |
| `framebuffer_*`, `hdmi_force_hotplug=1` | Harmless leftovers: they do **not** make the Pi 5 firmware publish a framebuffer node (see [Display](#display-the-videocore-mailbox-framebuffer)). |

## Updating the kernel on a prepared card

Only the kernel on the FAT partition needs rewriting between iterations; the F2FS root is untouched.
For a card prepared with `--boot-mode direct`, copy the fresh flat image over `kernel.img`:

```sh
./scripts/build-all.sh --arch aarch64 --rpi5
cp target/final-aarch64/kernel-direct.bin /Volumes/RPI5BOOT/kernel.img   # macOS mount point
```

No root privileges are needed on macOS for this. Keeping the stock `kernel_2712.img` on the card as a
known-good fallback is worthwhile (see [lessons](#lessons-from-the-bring-up)).

`scripts/deploy-rpi5.sh` is the older update path the README documented:

```sh
sudo ./scripts/deploy-rpi5.sh \
    target/final-aarch64/kernel-direct \
    /dev/mmcblk0
```

It mounts partition 1, copies the given file as `kernel.elf`, and rewrites `config.txt` to
`kernel=kernel.elf`. That predates the bring-up finding that the Pi 5 firmware takes a flat image,
not an ELF, so on a card prepared in direct mode it replaces a working configuration with one that
points at an ELF. Until it is updated, prefer the copy above.

## The serial console

The console is the **dedicated 3-pin JST debug connector** (BCM2712 `uart10` at physical
`0x10_7D001000`), 115200 8N1, driven by `arch/aarch64/src/uart.rs`. It is **not** the GPIO 14/15 UART:
that one lives inside RP1, behind PCIe, and is not used as the console (RP1 is brought up later in
boot, and a console must work before that). Any 3.3 V USB-serial adapter works; on Linux the reading user typically needs to
be in the `uucp`/`dialout` group.

On `rpi5`, `uart::reinit` **inherits** the firmware's programmed divisors (`IBRD`/`FBRD`/`LCRH`)
instead of computing them. The measured values were `IBRD=24, FBRD=0`, i.e. a UART clock of about
44.2 MHz — neither the DTB's `clk_uart = 9.216 MHz` (which would give `IBRD=5`) nor QEMU's 48 MHz
(`IBRD=26`). Do not "fix" this back to a constant.

## How the Pi boots

1. Boot ROM → `start4.elf` → `config.txt`; the firmware loads `kernel.img` at `0x200000`, the initrd
   at `0x10000000`, and passes the BCM2712 device tree.
2. The direct-boot entry stub (`_start`, placed at image offset 0 by `KEEP(*(.text.boot))`) drops
   EL2 → EL1, builds early page tables, enables the MMU **and the caches**, and enters `kernel_main`.
3. `kernel_main` installs the exception vectors before its first MMIO touch, so an unbacked address
   produces an `ESR`/`FAR` line naming it rather than minutes of silence.
4. The GIC-400 is driven as GICv2, the mailbox framebuffer is requested, and the SDHCI driver brings
   up the card. The BCM2712 `pcie2` root complex and RP1 are brought up by the board code (no ECAM:
   `BOOT_INFO.pci_ecam_base = 0`), unless `pcie.rpi5=0` (see [PCIe and RP1](#pcie-and-rp1)).
5. Init mounts F2FS from partition 2, `pivot_root`s, and starts the serial getty.

Pi builds stay GICv2-only by `cfg` (GIC-400); the GICv3 detection used on QEMU virt is compiled out.
See [kernel/boot.md](../kernel/boot.md) and [kernel/arch-aarch64.md](../kernel/arch-aarch64.md) for the
generic parts of this sequence.

## Bring-up: six bugs QEMU could not show

On 2026-08-20 the direct-boot path met real silicon for the first time. Every one of these bugs was
invisible in QEMU **by construction**, not by luck — each is a place where the emulator models
something away (`390eccd`):

| # | Bug | Why QEMU hid it | Fix |
|---|---|---|---|
| 1 | `_start` was not at image offset 0. The linker kept `*(.boot)` but the stub's section is `.text.boot`, so it fell into `*(.text*)` 0xbf8 bytes in, with the exception vectors at offset 0. | `-kernel` loads the **ELF** and jumps to `e_entry`; the Pi loads a **flat** binary and jumps to byte 0. | `KEEP(*(.text.boot))` first. |
| 2 | `kernel_address=0x40080000` is unloadable. | That address is QEMU virt's RAM base. | Link and load at `0x200000`; initrd at `0x10000000`; both below 1 GiB. |
| 3 | Early page tables covered only 0–4 GiB, with 0–1 GiB as Device memory. The test board has 16 GiB. | QEMU virt's memory map, not the board's. | PUD entries 4..15 (Normal), 64/65 (Device, for `uart10`, GIC-400 and SDHCI); 0–1 GiB Normal on `rpi5`. |
| 4 | UART divisor computed from the wrong clock. | Peripheral clocks are whatever the model says. | Inherit the firmware's divisors. |
| 5 | **Caches were never enabled** — the stub set `SCTLR_EL1.M` but not `C`/`I`. AArch64's exclusive monitor needs cacheable memory, so `LDXR`/`STXR` never succeeds and the **first `spin::Mutex::lock()` spins forever** (it was in `mm::buddy::reserve_range`). | TCG implements exclusives regardless of cacheability. | Enable `C` and `I`. This path could never have worked on any real ARM core. |
| 6 | **Non-cacheable page-table walker.** With caches on, `TCR_EL1 = 0x280100010` still declared table walks Non-cacheable and non-shareable, so the CPU wrote PTEs into L1 through a write-back mapping while the walker read around the cache and saw stale zeros. Every mapping made *after* the cache enable was invisible: the kernel ran, userspace never started — an infinite silent fault loop with `far == elr ==` init's entry and DFSC 0x04. | TCG has no caches. | `TCR_EL1 = 0x2B5103510` (write-back write-allocate walks, inner-shareable). |

Bug 6 is half of bug 5: enabling the data cache without telling the table walker about it is worse
than leaving both off, because the earlier tables still work and the failure moves somewhere
unrelated. The console survived only because the PUD-65 *block* mapping happened to cover the PL011.

Two further fixes landed with the bring-up:

- **PCIe must be off on `rpi5`.** BCM2712 publishes a `pcie@120000` node, so the DTB yields an ECAM
  base, but the controller needs link training this kernel does not do. Scanning it reads unbacked
  Device memory (runs of `0xDEAD`/`0xD0D0` vendor IDs) and raises **asynchronous** external aborts.
  Those sit pending behind `DAIF.A` until the scheduler unmasks for userspace, then all land at once
  as `[EXC] ESR=0xBE000011` (SError) with `ELR` at the userspace entry point — **implicating init,
  which is innocent**. The cost is the RP1 devices (USB, Ethernet), none of which have drivers yet.
- **`--rpi5` did not compile**: `sdhci` lacked the `read_blocks`/`flush` that `servers/f2fs` reaches
  through the `drivers::blkdev` alias.

## SDHCI

`drivers/src/sdhci.rs` is a polled PIO driver for the SD Host Controller (SDHCI Simplified
Specification 3.0). On `rpi5` it drives `mmc@fff000` (`brcm,bcm2712-sdhci`, physical
`0x10_00fff000`) — not `mmc@1100000` (the on-board SDIO Wi-Fi) or the disabled legacy `mmc@1108000`.
Its public API mirrors `virtio_blk` (`init`, `read_block(s)`, `write_block`, `flush`, `has_f2fs`,
4096-byte blocks), so `drivers::blkdev` selects between the two at compile time and nothing else in
the kernel changes. On Pi builds `drivers/src/blkdev_sdhci.rs` re-exports it with the registry's naming
helpers (`vdX` names, root = index 1); runtime registration of other disks returns `Unsupported`.

### Bugs found on real hardware (`bbf2a63`)

- **The `ACMD41` argument was `0x5100_0000`.** Its comment claimed "HCS | 3.3 V window" but it set bit
  24 (S18R) and an **empty voltage window**. Per the SD spec, an ACMD41 with no VDD window is an
  *inquiry*: the card reports its OCR and stays idle, the busy bit never clears, and the poll loop
  cannot terminate. It is now `0x40FF_8000` (HCS + 2.7–3.6 V). The correct window came from the
  firmware itself, which prints the card's OCR (`c0ff8000`) during its own probe.
- **Interrupt status was never cleared before issuing a command.** The wait helper cleared only the
  bits it waited for, so a stale Transfer Complete satisfied the *next* transfer's wait instantly;
  host and card desynchronised and every later command timed out. `send_command` now clears the
  normal and error status registers first.
- **No error recovery.** SDHCI 3.0 §3.10 requires a CMD+DAT line reset after an error; without it the
  inhibit bits stay asserted and one bad sector poisons every access afterwards. A failure now issues
  `SWRST_CMD | SWRST_DAT`.
- **Unbounded waits.** Every wait was a bare `loop`. All are bounded (`SPIN_LIMIT`) with stage markers;
  a lowercase marker on the console means that stage timed out.
- Diagnostics latch `(NORMAL << 16) | ERROR` *before* clearing — the first version read them after
  they had already been cleared and always printed zero.

### Bus speed negotiation (`1efd015`)

Identification runs at the slowest clock (about 390 kHz on the Pi), at which `brush` takes around two
minutes to demand-page in. After `CMD7`/`CMD16`, `negotiate_bus` climbs a **verified ladder**: `CMD9`
(CSD `TRAN_SPEED`), `ACMD51` (SCR), `ACMD6` to 4-bit, clock rungs, then `CMD6 SWITCH_FUNC` for high
speed, validated on the **64-byte DAT payload** (`status[16] & 0x0F == 1`), not on R1.

- **Every rung re-reads sector 0 and compares it** with a reference taken at the slow known-good
  setting. A misreported base clock makes the host run at twice what it thinks, and the controller
  reports nothing — only a data comparison catches it.
- Negotiation only ever **reads**, so a rung the bus cannot hold cannot corrupt the card.
- The divider is 10 bits split across register `0x2C` bits [15:8] and [7:6] (divisor `2N`), written
  as a **whole word**, never read-modify-write.

QEMU `raspi4b` result: `width=4 clk=26000kHz hs=1 base=52000kHz tran=25000kHz specver=2`; all four
degradation paths still boot, including total refusal falling back to the 101 kHz identification
floor. Expected effect on the Pi: roughly two minutes → about one second for that `brush` load.
Multi-block transfers were added afterwards and are covered below.

### Multi-block I/O, the tick stall, and the `sdhci.multi` knob

- **Multi-block (`pisdperf`).** Contiguous spans can go out as one `CMD18`/`CMD25` with Auto-CMD12
  (Transfer Mode bit 2, multi bit 5), up to 128 sectors = 64 KiB per command, and
  `blkdev_sdhci::write_blocks` now passes a whole span down instead of 4 KiB calls. Any multi-block
  failure resets CMD/DAT, aborts with a manual `CMD12`, and redoes the span sector by sector; three such
  fallbacks disable multi-block for the rest of the boot. There is still no DMA (SDMA/ADMA2 were not
  attempted); PIO multi-block under 10 MB/s on hardware is the stated trigger for that next step.
- **It is off by default.** On a Pi build with PCIe/RP1 up, inbound SSH was refused (RST: no listener)
  although sshd logged ready; the multi-block change was the only difference from the last working
  kernel, so `lane/pisd-multi` made single-block the default. The root cause was not confirmed on the
  Pi. `sdhci.multi=1` re-enables the multi-block path; the boot line says which mode is active
  (`[sdhci] I/O mode: PIO single-sector CMD17/CMD24 (multi-block off; sdhci.multi=1 enables it)`).
  `sdhci.multitest=1` runs a single-vs-multi read comparison self-test at probe.
- **Tick stall (`pitickstall`).** The PIO loops used to run with interrupts masked long enough that Pi CPUs
  went without timer ticks; the driver now opens IRQ windows per sector and every 1024 spins of its polled
  waits, and takes the DEVICES lock per block on reads.

## Display: the VideoCore mailbox framebuffer

**The Pi 5 firmware publishes no `simple-framebuffer` node, and `config.txt` cannot make it.** Its
DTB has a node named `fb` with `compatible = "brcm,bcm2708-fb"` and only a firmware phandle — no
`reg`, width, height or stride. Linux's driver gets the surface from the VideoCore mailbox, so it is
deliberately absent from the device tree. `framebuffer_width/height/depth` plus `hdmi_force_hotplug`
were tested and still gave `Physical base: 0x0, Resolution: 0x0`. (Dumping every FDT node name from
`kernel_main` — 242 on the board — settled this after two boots spent guessing node names.)

So the framebuffer has to be asked for (`f480fbe`). `drivers/src/rpi_mailbox.rs` implements the
VideoCore property interface:

- **Mailbox address from the board's own DTB**: `soc/ranges` applied to `mailbox@7c013880` gives
  `MBOX_BASE_PHYS = 0x10_7C01_3880` (inside the PUD-65 Device block), channel 8. Bus addresses are
  `phys + 0xC000_0000` per `soc/dma-ranges`, which stays valid on the 16 GiB board; only the property
  buffer must be below 1 GiB.
- The property buffer is a 64-byte-aligned static in `.bss`: at that point of boot there is no buddy
  allocator, no heap, and the Normal-NC memory attribute is not yet installed.
- Tags: set physical/virtual size, depth, pixel order, allocate buffer, get pitch. It requests a
  **virtual height of twice the physical** so a later panning implementation needs no reallocation,
  and falls back to a plain buffer (`RELEASE_BUFFER` + retry) if the firmware refuses.
- **Mapping.** The surface lands in 0–1 GiB, which the entry stub maps as one cacheable block that
  `map_4k` will not split. Rather than teach `ensure_table` to split blocks, the framebuffer is mapped
  at a dedicated VA, `0xFFFF_A000_0000_0000` — L0 slot 320, which the stub leaves empty — so fresh
  tables are built with the Normal Non-cacheable attribute.
- **Cache maintenance to the Point of Coherency.** The existing `arch_flush_cache_range` is
  `dc cvau` (Point of Unification), which is insufficient for a non-coherent DMA master like the
  scanout engine; `arch_dcache_clean_inval_range` (`dc civac`) was added. Substituting the old one
  produces stale-pixel corruption that looks like a mapping bug.
- Mapping, `reserve_range` and the cache sweep all go through **one** `BootInfo::framebuffer_bytes()`
  — the first version swept the full virtual allocation while mapping only `pitch × phys_height`,
  and faulted on the buffer's last word.
- Every receive loop has a fixed deadline. A drain loop that re-armed its deadline every iteration
  hung the boot silently and indefinitely on an all-zeroes mailbox window; it was found only by
  deliberately exercising the timeout path.

Fault injection in QEMU proved firmware clamping (asked 640×30720, got 640×2560, with mapping and
sweep tracking the clamped value), the tall → plain fallback, and a timeout degrading to serial-only
while still reaching `login:`.

**Still hardware-only:** whether the Pi 5 firmware honours `ALLOCATE_BUFFER` at all, the real HDMI
mode, pitch and pixel order, and Normal-NC coherency against a real scanout. The success trace on the
serial console, in order, is `[MBOX] base=` → `smoke code=0x80000000` with sane firmware revision and
board → `fb code=0x80000000 … virt=1920x2160` → `pitch=/tight=/v3d_linear_match=` →
`fbrb first=0xA5A55A5A last=0x5A5AA5A5` → colour bars on screen. `alloc_bus=0x00000000` or
`code=0xE0000003` means the firmware declined the tags (boot continues serial-only); `TIMEOUT` means
the address is backed but the VideoCore is silent; `[MBOX] base=` as the last line means a wrong
address, named by the following `[EXC] FAR=`. The leftmost colour bar must be **red**; if it is blue,
flip `PIXEL_ORDER_BGR` in `rpi_mailbox.rs` — serial cannot tell you this. HDMI must be plugged into
HDMI0 at power-on, because the firmware probes only then. `BAR_HOLD_SECS` in `kernel/src/main.rs`
(3 s) holds the test pattern and can go to 0 once settled.

Honest scope: BCM2712 has **no 2D blit or fill engine** — the HVS composites planes, it does not
rasterise. Console text, rectangle fills and glyphs stay on the CPU at every stage, and that is the
correct design, not a shortfall. Hardware can take scrolling (mailbox `SET_VIRTUAL_OFFSET` panning),
page flips and the cursor, plus all of OpenGL through V3D. The 1.58–1.82 s per line scroll figure in
an old `framebuffer.rs` comment predates the write-back shadow buffer and is stale; a scroll is now a
cached memmove plus a write-only blit, single-digit milliseconds. See
[graphics/console.md](../graphics/console.md).

## GPU acceleration groundwork

The V3D path is designed and partly built; details of the Mesa side are in
[graphics/mesa.md](../graphics/mesa.md). Kernel pieces that exist:

- **Interrupt dispatch table** (`d84f5f8`): `arch/aarch64/src/gic.rs` gained `request_irq(id, f)`
  (register-then-enable, the only safe order), `enable_spi`/`enable_spi_edge`/`disable_spi` and a
  lock-free `[AtomicUsize; 512]` dispatch table. **512, not 64** — GIC-400 implements up to 480 SPIs
  and BCM2712's numbering reaches into the 200s. An unhandled SPI is masked after one log line,
  because an unclaimed level-triggered SPI otherwise livelocks the CPU.
- **DRM syncobj** (binary; timeline deliberately `ENOSYS` with `DRM_CAP_SYNCOBJ_TIMELINE = 0`,
  because a scalar approximation smoke-tests clean and then deadlocks Mesa), with an ISR-callable
  retire hook as the V3D seam.
- **A V3D ioctl surface** (`75cdbee`) with which Mesa walked its entire screen-creation contract
  in QEMU. V3D's ioctl numbers collide bit-for-bit with virtio-gpu's (`V3D_WAIT_BO == VIRTGPU_MAP`),
  so it is a separate dispatch selected per backend, never extra arms in the same `match`.
- Mesa 25.3.6 builds with the `v3d` gallium driver for aarch64 (`5a40318`); it needs no LLVM.

> **Superseded by the [V3D](#v3d-gpu) section below**, which records what was built and proven on hardware.
> The paragraph that follows is the original plan.

The remaining V3D work (GPU MMU, job queues and binner overflow, the ISR — an estimated 3,300–4,300
lines) is hardware-in-the-loop only: QEMU models no V3D anywhere. It is staged behind two cheap
milestones: reading `V3D_HUB_IDENT0` back as ASCII `"V3D"` with tech version 7 (proves discovery,
mapping, power domain and clock in one register read), then a deliberate MMU fault at VA 0 caught by
the ISR (proves page tables, MMU enable and the interrupt before any command list exists).

## PCIe and RP1

The Pi 5's USB, Ethernet, GPIO UART, fan PWM and LEDs all sit in the RP1 south bridge behind the BCM2712
`pcie2` root complex. It is `drivers/src/brcmstb_pcie.rs` (pure sequencing in `brcmstb_hw.rs`, host-tested)
plus `rp1.rs`, and as of `lane/pipcie-default` (2026-10-28) it is **on by default**; `pcie.rpi5=0` opts
out (the boot prints `[pcie] off (pcie.rpi5=0)`). `rp1.irq` stays default off. All RP1 registrations
(kexec quiesce hooks, USB controller list, `eth0`, fan) are reached only through `rp1::probe`, which runs
only when the link came up, so a failed bring-up registers nothing. The 2026-10-28 matrix on the Pi
(kexec trial to trial x3, `pcie.rpi5=0`, return to the previous kernel, 28-minute soak) found no bus
corruption, watchdog or panic.

Real-silicon findings that shaped it:

- **The firmware leaves the x4 controller in reset** (`pciex4_reset=1`), so a down link at hand-over is
  normal. An early stage-5 stall (`pipcie3`) was an RC register read-modify-write (`perst(true)`) issued
  while the bridge reset held the RC block: the AXI read never completes. Between assert and de-assert of
  the bridge reset only the reset block and a short delay are touched, as in Linux `brcm_pcie_setup()`.
  Each bring-up stage is announced on the raw UART before its first access (`pipcie2`), so a stall names
  its stage; `pcie.stop=N` stops after stage N and `pcie.reuse=1` reuses a link the previous kernel left up
  (for hand-over experiments).
- **RP1 BARs read all-ones** (`chip_id=0xFFFFFFFF`) because BAR sizing used sub-dword config writes, no BAR
  was placed, and the root-port memory window was therefore written closed. BARs are now sized with dword
  writes, placed, read back, and the window opened over them (`pirp1-bar`).
- **Bus mastering never turned on** because sub-dword config writes did not land; they are now dword
  read-modify-writes (`pigem-data`), which is what actually enables DMA for GEM and xHCI.
- **Payload size.** The root complex splits read completions at its own MPS while RP1 resets to 128 B, so
  256-byte DMA reads came back as malformed TLPs. RP1's MPS/MRRS now follow the root port (`pigem-tx`).
- **No bus master survives a kexec** (`pikexecdma`). The RP1 GEM kept receiving into pages the next
  kernel's buddy allocator had already handed out, which showed up as `[BUDDY] CORRUPT LINK` and as an
  EPOLL null write. Each engine is now stopped at its own registers (`dma_shutdown` hooks: RP1 GEM, xHCI,
  dma40), then `pcie2` is cut with PERST# and a bridge reset, and the next kernel starts with an early reset
  (`[pcie] early reset`, `[kexec] quiesced: dma40 xhci0 xhci1 gem pcie-perst`).
- RP1 clocks (`rp1_clk.rs`: PLLs read back, never reprogrammed; `clk_eth` 125 MHz, `clk_eth_tsu`/`clk_pwm1`
  50 MHz enabled only if off), RP1 GPIO (`rp1_gpio.rs`: function, pull, RIO output/input, active-low reset
  pulse) and the PWM fan, ACT/PWR LEDs and the KEY_POWER button (`pirp1`) hang off the same probe.

### Ethernet (GEM / macb)

`drivers/src/macb.rs` (+ `macb_hw.rs`, host tests in `drivers/macb-tests`) drives the Cadence GEM in RP1
with the BCM54213PE PHY (clause-22 MDIO, RGMII delays as Linux `bcm54xx_config_clock_delay`) and
registers `eth0` like r8169/e1000e. It is attached only by `rp1::probe`, and not at all if a GEM clock
cannot be enabled. Hard-won details:

- **Descriptor rings are Normal non-cacheable by default.** After a kexec the FDT is `linux,dummy-virt`,
  which made the non-coherent-DMA detection (which looked for `brcm,bcm2712|2711` in the DTB) come up
  coherent; rings then sat in write-back memory and RX stuck at slot 0 while the GEM reached slot 115.
  The kexec FDT now carries the non-coherent mode, and `macb.dma=n` selects the ring mapping explicitly
  (`pigem-coh`). Descriptor pages are invalidated behind the NC alias and a `dsb` precedes `TSTART`
  (`pigem-ring`).
- **TX recovery.** TSR `tfc|hresp` on large frame reads (see the MPS fix above), halted-transmitter
  recovery, `macb.burst=` / buffer knobs (`pigem-tx`), a tail-keyed watchdog and resync (`macb.fix1=0`
  disables; default on), and an opt-in polled `TSTART` re-ring (`macb.fix2=1`, default off: it did not help
  and issued thousands of spurious `TSTART`s). `/proc/net/nic` shows hardware counters.
- **Bulk send stall was above the NIC** (`pigem-bulk` refuted tail desync and lost `TSTART` on hardware).
  The vendored smoltcp 0.11 had no congestion window, so every `eth0` burst was a full 256 KiB window.
  `lane/pitcp` added Reno and CUBIC (`tcp.cc=none|reno|cubic`, default **cubic**; also `tcp.iw`,
  `tcp.rto_min_ms`, `tcp.sndbuf_kb`, `net.cadence_us`), `/proc/net/tcpsnd`, and `netd_pass_hist:` in
  `/proc/net/nic`. The lane records `eth0` bulk send going from 0.3 to 29 MB/s.

### USB (RP1 DWC3)

`dwc3_hw.rs` / `rp1_usb.rs` (`piusb`) set up RP1's two DWC3 cores in host mode (GCTL, core and PHY reset,
Linux's `axi-pipe-limit`, TX threshold and GUCTL1 quirks), turn VBUS on through RP1 GPIO 42 and hand each
to the existing xHCI driver with a DMA offset (`rp1::dma_addr`, `usb::platform::set_dma_offset`) because
RP1 sees system RAM at a different bus address. DMA buffers come from non-cacheable coherent `DmaBuf`s,
since this board is not DMA-coherent. The controllers join `usb_hcd`'s list so `quiesce_all` halts them
before a kexec. A failed endpoint is recovered by disabling only its slot (`xhcisafe`), not the controller.

## Wi-Fi (CYW43455 / brcmf)

The on-board CYW43455 sits on `sdio2` (`drivers/src/brcmf/sdio_host.rs`, a PIO-polled SDHCI host with no GIC
line) and is driven by `drivers/src/brcmf/` (chip, firmware, SDPCM, BCDC, `cfg.rs`, `netdev.rs`;
host tests in `drivers/brcmf-tests`). It exposes `wifi0` through the same radio manager as iwlwifi
(`wifi_mgr`; `/etc/leandros/wifi.conf` autoconnect and netctl profiles; `wifiscan` and `/proc/net/wifi` show
the BSS cache). The firmware (`cyw43455-sdio.bin`, CLM, NVRAM) is staged into the root by
Alpine's `linux-firmware-brcm`/`-cypress` (`ports/alpine/firmware.list`); `prepare-rpi5-sdcard.sh --wifi-ssid NAME` writes the credentials (the SSID is
never derived from the PSK file name: SSIDs are case-sensitive, and an early card used the wrong case).

**Multicast.** The CYW firmware drops group-addressed frames unless told otherwise, so `init` sets the `allmulti` iovar after the interface is up (lane nicmcast). Without it mDNS and IPv6 neighbour discovery never reached the stack (a Mac sending 30 datagrams to 224.0.0.251 produced +5 received frames before, +37 after). There is no per-group list: the chip passes all multicast and the network stack filters by joined group. The other NIC drivers already pass multicast (macb hash all-ones, e1000e `RCTL_MPE`, r8169 `ACCEPT_MULTICAST`, iwlwifi's pass-all `MCAST_FILTER_CMD` after association; `iwl.mcast=0` disables it).

How the bring-up works and why it looks the way it does:

- **Download order follows brcmfmac.** ALP clock only while downloading, KSO wake, CR4 halted and D11
  reset, code at RAM base `0x198000`, NVRAM (with its length-token word) at the top of RAM, both read back,
  CR4 released, then the HT clock requested with a **1 s** bound. The first version asked for HT before the
  write with a 15 ms bound and failed with `fw HT clock timeout` on a fresh chip.
- **Read-back must be word aligned.** With the 2_4B backplane flag the SDIO core drops address bits 1:0,
  so the old unaligned 256-byte tail sample (offset 609053 of a 609309-byte image) read shifted bytes
  and reported a mismatch although the write was fine (`pibrcmf-dl`). `peek`/`poke` and SDIO/download
  retry knobs let a boot be bisected over serial.
- **Runtime control file `/etc/brcmf.ctl`** is serviced once a second in every state (`status`, `scan`,
  `join [ssid=..]`, `diag [lines=N]`, `retry [knobs]`); `retry` winds the manager down and re-probes with a
  `WL_REG_ON` cycle. `/proc/net/nic` shows `wifi0` state, caps, join options and the RSN fields below, and
  `/proc/klog` plus `dmesg` carry the `[brcmf]` lines.
- **The firmware has no `idsup`, so the host runs the 4-way handshake.** Linux enables the in-firmware
  supplicant only when `cap` lists `idsup`; this 7.45.265 build does not (the early symptom was
  `BCME_UNSUPPORTED -23` on `sup_wpa`/`WLC_SET_WSEC_PMK`). `wifi_crypto::Supplicant` (shared with
  iwlwifi) takes the EAPOL frames, answers message 2/4 through the firmware data path, and installs PTK and
  GTK with `wsec_key`.
- **AKM 6 / MFP.** The AP's RSN element is decoded at join. The auto choice is AKM 6 (SHA-256 KDF, AES-CMAC
  MIC, key descriptor v3, `wpa_auth` `0x8000`) if offered, else AKM 2 with MFPC if the AP and firmware
  both support it. IGTK is installed at key index 4/5. Knobs (all on `join`): `akm=auto|2|6`,
  `mfpc=auto|0|1`, `mfpr=auto|0|1`, `msg2ie=auto|ours|fw`, `wpaie=late|early|0`.
- **Message 2 echoes the RSN IE the firmware actually put in the association request**
  (`assoc_info` / `assoc_req_ies`). The firmware sets the PTKSA-counter bits itself, so an IE built from
  the profile differed and the AP rejected the handshake. Reading `assoc_info` into 8 bytes had returned
  `BUFTOOSHORT` on every join; with a 512-byte buffer 5 of 5 trial kexecs associated (`piwifiap`).
- **Failure classification.** A deauth with reason 1/2 and no message 1 re-send after message 2 is an AP
  rejection (BSS back-off, profile stays eligible), not a wrong key (`piprofile`). The manager keeps a
  per-boot rejected-BSS list with a 10-minute decay, and a profile is marked wrong-key only after verdicts
  on `wifi.wrongkey_bss` (default 2) distinct BSSes within 10 minutes (`piwifikexec`: one weak-BSS
  handshake timeout at -81 dBm had blocked the profile for the whole boot). 5 GHz is preferred
  (`wifi.prefer5=1`); `netctl watch --stamp`, started by init, stamps profiles on connect.
- **Kexec.** The CYW43455 keeps running across a kexec, and the kexec FDT has no `wifi@1` node. The kernel
  therefore powers the chip down on kexec, restart and power-off (disassoc, `WLC_DOWN`, host reset,
  `WL_REG_ON` held low across the jump), holds `WL_REG_ON` low for at least 100 ms at probe (200 ms default),
  and auto-retries once with 1 s off after a `chip`/`fw`/`clm`/`ver` failure. The MAC falls back to
  `brcmf.mac=` or the firmware's `smsc95xx.macaddr=` + 1 when neither the DTB nor the OTP tag is available.
- **Wedges.** A netd spin with interrupts off on a not-yet-built stack (`pirxwedge`), unbounded credit and
  control waits under steady RX (`piwifi-up`), and the SSH listener not being adopted by a late-built
  `wifi0` stack (`pisshrace`) were each fixed; the `piwedge` raw-UART diagnostics (`brcmf dp on|off|rx|yield`
  bisection, IRQ trail, stage markers) remain for the next one.

Open: BSS `84:78:48:d8:7e:79` rejected with reason 2 on every boot in the last lane note; WPA3 is deferred.

## DMA, GPIO and HDMI audio

- **dma40** (`drivers/src/bcm2712_dma40/`, `pidma40`): the BCM2712 40-bit DMA engine, with a pure control-block
  layer (32-byte `Scb`, memcpy chains, cyclic memory-to-FIFO rings with `D_DREQ`), interrupt or polled
  completion, channel selection and a boot self-test. Its DREQ numbers for hdmi0/hdmi1 depend on the stepping and come from the
  two DTBs: C0 `0x41fa000a`/`0x41fa0011` = 10/17, D0 (`bcm2712d0-rpi-5-b.dtb`) `0x41fa000c`/`0x41fa000d` = 12/13
  (`cb::hdmi_dreq_cell`; `hdmi.dreq=N` overrides). The only FIFO address is the CPU physical MAI_DATA; there is no 32-bit
  bus alias (it would be DRAM on >= 2 GiB boards).
- **GPIO / pinctrl / l2 intc** (`bcm2712_gpio.rs`, `pigio`): brcmstb `gio` and `gio_aon` banks, `pinctrl` tables
  for the C0 and D0 steppings generated from Linux by `scripts/gen-bcm2712-pinctrl.py`, and chained l2
  interrupt controllers. The stepping always comes from the chip id (SUN_TOP_CTRL); an unknown stepping
  refuses pin mux. D0 is what the Pi ships with; the card carries the BCM2712 D0 device tree and overlay
  (`pid0`, `pid0c`).
- **HDMI audio** (`drivers/src/hdmi_audio/`, `pihdmi-audio`): IEC958 subframes, MAI/ACR registers (the D0
  7-bit `VC6_D` threshold layout), audio InfoFrame, fed by a dma40 cyclic ring on hdmi0, registered as a
  PipeWire backend. A read-only probe (`hdmi.probe=0` disables) decides HDMI vs DVI, measured pixel clock
  and the audio gate first (`pihdmi-probe`). The lane notes record the path as host-tested; they do not
  record audible output.

## V3D GPU

The kernel V3D 7.1 driver (`drivers/src/v3d/`, design in `artifacts/notes/lane-piv3d-design-2026-10-26.md`)
was built in phases and armed from the Pi 5 probe: power and identification, MMU page tables and IRQ lines,
a real `SUBMIT_CL` queue (out-of-memory binner overflow, hang reset, multi-sync), `SUBMIT_TFU` and
`SUBMIT_CSD`, and scanout by a pitch-converting copy from a v3d BO into the firmware framebuffer
(`kms_v3d.rs`). `v3d.probe=1` climbs the whole ladder on the Pi and prints the identification line
(`ver=71 rev=10 cores=1 qpus=12 va=36 pa=36 clk=960MHz` in the lane note).

- **The power gate was wrong.** It required PM_GRAFX `POWOK`, but the 2712 firmware leaves GRAFX at
  `0x706d` (`POWUP|ISPOW|ISFUNC`, `POWOK` and `MRDONE` clear) with V3D answering behind it. The gate now
  requires the three bits that are set. The firmware's `SET_DOMAIN_STATE` is rejected on this board
  (`BadCode`); `v3d.pwr=fw` (off by default) tries it and `v3d.pwr=nogate` skips the island check.
- **Mesa `v3d` renders GLES on the Pi** (`pigpu2`). It had fallen back to softpipe because the Pi root still
  carried the virtio-pci sysfs identity for card0/renderD128 and `pi-sync` skips `/sys/dev/char/226:*`.
  `scripts/pi-v3d-sysfs.sh` rewrites the identity to a platform device once per root. `es31test --fbo`
  and `vkprobe` ship in the aarch64 set. kmscube has no display path on the Pi yet.
- `kexec` quiesces V3D (`pikexecdma2`); `V3D` is a tristate (`y`/`m`/`n`) in the kernel-module work
  ([kernel/modules.md](../kernel/modules.md)). Proven on the Pi: `=y` and `=m` both run kmscube at about 60 fps, `vkprobe` and `es31test` pass; with `=m` the kernel image contains no `v3d_real`/`ClQueue`. `scripts/pi-gpu-smoke.sh` scores 5/5 on `=y` or `=m` (run it with no compositor holding card0).

## Board support: watchdog, panic, fan, identity

- **Watchdog** (`piwdt`). The BCM2712 PM watchdog (`brcm,bcm2712-pm`, `0x10_7d200000`) is armed at boot with
  its 15 s maximum and fed by cpu0's tick scan only while every busy CPU is still ticking; a CPU stuck with
  interrupts masked therefore lets the board reset itself roughly a minute into the stall. `wdt.off=1`
  disables it. A serial reset (5x Ctrl-] or a BREAK) is also provided. After a kexec the FDT has no PM
  node, so arming now keys off the BCM2712 chip id plus a PM register sanity read (`piwdtdt`).
- **Panic reset** (`piwdtpanic`). A panic used to leave the watchdog fed (the panicking CPU kept ticking).
  The panic handler now stops feeding and resets the board after `panic.reset=<s>` seconds (default 5; 0 =
  halt). `wdog.selftest=1|2` exercises the stuck-CPU and countdown paths.
- **Fan** (`piboard`, `rp1_fan`): the curve follows the DTB cpu-thermal trips, ticks every second, forces full
  speed at 85 C; `fan=auto|off|0..4`. `/proc/thermal` has a `fan:` line with state, level and RPM
  (the lane saw 40.2 C at state 0 and 52.9 C at state 1, level 75/255).
- **Identity.** The Bluetooth address is chosen by `bt.bdaddr=auto|dt|otp|keep|xx:xx:..`. On the Pi the
  OTP/firmware MAC tag fails (`[otp] mac=none`), so `auto` falls back to the Ethernet MAC from the firmware's
  `smsc95xx.macaddr` + 2 (Wi-Fi uses + 1). Changing the address invalidates existing bonds: re-pair once, or
  use `bt.bdaddr=keep`.
- **Misc** (`pimisc`): hardware RNG (`rng200`, boot seed plus reseed source), firmware RTC read at boot and
  written back by `clock_settime`, AVS thermal reading, an `ondemand`-style ARM clock governor, and a device
  inventory (`artifacts/notes/pi5-device-inventory-2026-10-26.md`).

## Updating a running Pi over the network

Once the Pi has network and sshd, no SD swap is needed. The board's FAT partition is not reachable from
LeandrOS, so kernels live in the root at `/boot/pi/{trial,good,prev}/{kernel,initrd,info}` and are entered
with `kexec`; the firmware always boots the card's `kernel.img` (the factory kernel), which this flow never
writes, so **a power cycle is the recovery** and `pi-boot good` returns to the promoted kernel.

```sh
export LEANDROS_REMOTE=pi LEANDROS_PI_HOST=<pi-ip>
./scripts/build-all.sh --arch aarch64 --rpi5
python3 scripts/remote.py pi-deploy        # upload to /boot/pi/trial, kexec, wait, check uname -v
python3 scripts/remote.py pi-status        # slots, running kernel, cmdline
python3 scripts/remote.py pi-promote       # trial -> good (only while running from trial)
python3 scripts/remote.py pi-boot good     # after a power cycle; --append is per boot, not cumulative
python3 scripts/remote.py pi-sync          # update the root fs in place from the current build
```

- `pi-deploy` refuses an ELF whose lowest PT_LOAD is not `0x200000`. The firmware command line is kept
  (`--reuse-cmdline`); `--append` adds arguments for that boot only.
- **`pi-sync`** (`piuserland`) builds the root manifest with `LEANDROS_ROOT_MANIFEST` (paths, modes,
  sha256, symlinks), diffs it against the live root (`stat` then `sha256sum`), and uploads changed files
  as verified bundles with per-file atomic rename. Bundles of 2 MiB (`LEANDROS_PISYNC_BUNDLE`) took 96 s for
  46 changes versus about 25 minutes with one 64 MiB bundle. It requires `rg`, `grep` and `dmesg` in the build
  (`LEANDROS_PISYNC_ALLOW_MISSING=1` overrides). `--dry-run`, `--list` and `--plan-out` preview a sync.
- **Kexec on the Pi** needed several board-specific fixes: the firmware DTB's reserved `atf@0` range (BL31) is
  carried; the initrd is placed at `0x1000_0000` where the rpi5 kernel looks; GICv2 SGIs are cleared;
  secondaries are **parked in a mailbox loop** instead of `PSCI CPU_OFF`, because the Pi's TF-A never
  restarts a core after `CPU_OFF` (the new kernel saw `1 CPUs online`); and every bus master is stopped
  first (see above). `pi-boot` runs `kexec -e` in the foreground because a backgrounded one died with the
  SSH channel's SIGHUP.
- **Serial fallback** (`piserialxfer`): `scripts/pi-serial-push.py <local> <remote>` pushes a file over the
  console to the in-guest `serrx` (framed, CRC-checked, go-back-N, sha256 verified, atomic rename), and
  `scripts/pi-serial-kexec.sh` pushes a kernel and initrd and kexecs. It refuses a port another process has
  open and never kills it.

## Init without a GPU, and background daemons

`lane/piinit`: init runs a GPU pre-check (`/bin/sh -c '. /bin/gpu-env ...'`, 120 s bound, skipped when
`dm.precheck=0` or when `/proc/nvrm_state` is not `absent`). Exit 78 prints a `NO GPU` banner once and
init does not start greetd or the stray sweep, which is the Pi's normal state until a display path exists.
`bthid` starts without `/dev/hci0`; `btd` and `netctl watch` are supervised daemons (see
[servers/bluetooth.md](../servers/bluetooth.md#supervision)). `userland/grep` is a grep/egrep/fgrep shim
over `rg`.

## The `raspi4b` QEMU path

`./scripts/build-all.sh --arch aarch64 --raspi4b` plus `./scripts/run-qemu.sh --raspi4b` boots the
kernel on QEMU's BCM2711 model (`-machine raspi4b -m 2G -smp 4`, fixed SoC, GIC-400). It exists
because no QEMU version models the Pi 5, and it makes two things testable for free: the SDHCI driver
against QEMU's generic SDHCI model, and the whole mailbox protocol (`bcm2835-property` +
`bcm2835-fb`). The split to remember is **QEMU proves the protocol, hardware proves the memory
system** — TCG has no caches, so every Normal-NC or `dc civac` coherency bug is invisible there.

The `raspi4b` SDHCI base address was wrong in the tree for a long time: `0xfe340000` instead of
`0xfe300000`. Both report an identical capability register (`0x052134b4`), which is why the empty
controller looked healthy and "CMD8 always times out" was blamed on QEMU's block backend. Reading
`PRESENT_STATE` (`0x…24`) at each address settled it: Card Inserted is 1 only at `0xfe300000`.
`dbg()` was also a silent no-op on `raspi4b` until it was pointed at BCM2711 UART0 (`0xFE201000`).

## Lessons from the bring-up

- **Green QEMU runs are evidence about logic, never about hardware.** When touching early boot,
  MMU/cache setup, linker scripts, load addresses, or any lock reachable before userspace, ask what
  QEMU emulates away. See [development/methodology.md](../development/methodology.md).
- **Bisect with a known-good vendor kernel early.** One boot of the stock `kernel_2712.img`
  validates the cable, connector, baud rate, card and firmware together; trying it late cost several
  cycles, and it is what exposed the refused-load signature.
- **A `Loading '<file>' …` line with no matching `Read <file> bytes …` line is a refused load.** The
  firmware never reads the file, so changing its header, size or flags makes no difference.
- **A filesystem that mounted is not a filesystem that is intact.** Early boots mounted the card's
  F2FS and issued checkpoint writes while the SDHCI controller was wedged. The volume still mounted
  and `pivot_root` succeeded, but directory lookups were broken, so `execve` failed for `/bin/login`,
  `/bin/shell` and `/bin/brush` while SD I/O reported zero errors. A full re-prep fixed it. When I/O
  has been failing, re-image before debugging further up the stack.
- Two hypotheses were checked and **refuted** during the hunt, and should not be chased again:
  kernel-image/userland VA overlap (user tasks get their own page-table root; the overlap is virtual
  only), and a missing framebuffer swallowing output (`console_write_user` goes straight to serial).
- The iteration loop is manual — the card moves physically between the build host and the Pi — so
  every diagnostic that saves a boot is worth adding: the early exception vectors, the bounded waits
  with stage markers, the `uart_2ndstage` narration, and the `EARLY_PUTC` macro kept (unused,
  documented) in the entry stub because it is the only way to see anything from there.

## Limitations and open work

- No display path for scanout GL apps (kmscube) on the V3D yet; desktop on the Pi is not there.
- HDMI audio output, the HDMI console and the SD speed negotiation have not been confirmed by ear/eye in the
  lane notes; HDMI console and SD ladder are QEMU-proven.
- SDHCI is PIO and single-sector by default; `sdhci.multi=1` is unproven on the Pi (see above).
- Wi-Fi: WPA3 deferred; one AP rejects with reason 2 every boot; weak-signal BSSes still produce handshake
  timeouts (now back-off, not a blocked profile).
- Bluetooth reconnect: the 8BitDo Lite 2 gamepad auto-reconnected on the good kernel `f51a46029ffb` (2026-10-28,
  user-confirmed); other pads are unverified; see
  [servers/bluetooth.md](../servers/bluetooth.md).
- Single block device: the SD card. There is no home partition on it, so the fstab's `LABEL=leandros-home`
  mount of `/home` fails (`nofail`) and `/home` is the root's own.
- `deploy-rpi5.sh` writes an ELF and a `kernel=kernel.elf` config (see above); the network flow replaces it
  for everything except `kernel.img` itself, which still needs a card swap.
- OTP/firmware MAC tag read fails on the Pi 5 firmware (`GET_BOARD_MAC_ADDRESS`); cause not found.

## Code and tests

- Build and card scripts: [scripts/build-all.sh](../../scripts/build-all.sh),
  [scripts/prepare-rpi5-sdcard.sh](../../scripts/prepare-rpi5-sdcard.sh),
  [scripts/deploy-rpi5.sh](../../scripts/deploy-rpi5.sh),
  [linkers/aarch64-direct.ld](../../linkers/aarch64-direct.ld)
- Drivers: [drivers/src/sdhci.rs](../../drivers/src/sdhci.rs),
  [drivers/src/blkdev_sdhci.rs](../../drivers/src/blkdev_sdhci.rs),
  [drivers/src/rpi_mailbox.rs](../../drivers/src/rpi_mailbox.rs)
- Arch: [arch/aarch64/src/uart.rs](../../arch/aarch64/src/uart.rs),
  [arch/aarch64/src/gic.rs](../../arch/aarch64/src/gic.rs),
  [arch/aarch64/src/smp.rs](../../arch/aarch64/src/smp.rs),
  [kernel/src/main.rs](../../kernel/src/main.rs) (`INITRD_PHYS`, PCIe off, mailbox, `BAR_HOLD_SECS`)
- RP1 / network / board: `drivers/src/brcmstb_pcie.rs`, `rp1.rs`, `rp1_usb.rs`, `macb.rs`, `dwc3_hw.rs`,
  `bcm2712_dma40/`, `bcm2712_gpio.rs`, `hdmi_audio/`, `v3d/`, `rpi_misc.rs`, `rpi_pm.rs`, `brcmf/`
  (host tests: `drivers/brcmf-tests`, `macb-tests`, `dwc3-tests`, `rp1-tests`, `bcm2712-tests`),
  `scripts/remote.py` (`pi-*`), `scripts/pi-serial-push.py`, `scripts/pi-v3d-sysfs.sh`
- Regression: the `raspi4b` boot (`run-qemu.sh --raspi4b`) for SDHCI and the mailbox; the aarch64
  `--direct` QEMU boot, the only QEMU path that exercises the entry stub's cache/TCR/PUD code (Limine
  branches around it). Real-board verification is a serial capture showing the prompt.

## See also

- [README.md](README.md) — targets and hosts
- [kernel/arch-aarch64.md](../kernel/arch-aarch64.md) — exception vectors, GIC, timer, PSCI
- [graphics/mesa.md](../graphics/mesa.md) — the `v3d` Mesa build and its kernel contract
- [servers/bluetooth.md](../servers/bluetooth.md) — `/dev/hci0`, `btd`, `bthid`, the Pi UART bring-up
- [kernel/modules.md](../kernel/modules.md) — Y/M/N gating of drivers
- [servers/filesystems.md](../servers/filesystems.md) — F2FS and the block layer
