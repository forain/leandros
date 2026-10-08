# Raspberry Pi 5

LeandrOS boots on a real Raspberry Pi 5 (BCM2712, Cortex-A76) from its SD card: firmware → kernel →
init → F2FS root on the card → `pivot_root` → getty → `login:` → an interactive `brush` shell, all
over the board's debug UART. This page covers the `rpi5` build, preparing and updating a card, the
bring-up bugs that only real silicon exposed, and the display and GPU groundwork that is proven in
QEMU but still waits for a hardware boot.

## Status

| Area | State |
|---|---|
| Boot to a shell prompt over serial | **Working** since 2026-08-20 (`bc9de03`..`b328db7`) |
| SD card (SDHCI) read/write, F2FS root | **Working**, zero transfer failures |
| SD bus speed negotiation (4-bit, high speed) | Implemented and proven on QEMU `raspi4b` (`1efd015`); not yet confirmed on the Pi |
| HDMI console via the VideoCore mailbox | Implemented and proven on QEMU `raspi4b` (`f480fbe`); not yet confirmed on the Pi |
| GPU (V3D) | Mesa builds with `v3d`; the kernel side is an ioctl stub — see [graphics/mesa.md](../graphics/mesa.md) |
| USB, Ethernet, Wi-Fi | **Not available**: they sit behind RP1 on PCIe, which is not brought up |
| SMP | Four cores via PSCI. CPU enumeration moved to the device tree in 2026-10 (`963c681`); not re-verified on the Pi since |

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
that one lives inside RP1, behind PCIe, and is unreachable from a bare-metal kernel that does not
train the PCIe link. Any 3.3 V USB-serial adapter works; on Linux the reading user typically needs to
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
4. PCIe probing is forced **off** (`BOOT_INFO.pci_ecam_base = 0`), the GIC-400 is driven as GICv2,
   the mailbox framebuffer is requested, and the SDHCI driver brings up the card.
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
Not yet done: multi-block `CMD18`/`CMD25` — each 4 KiB block is still eight single-sector `CMD17`s.

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

The remaining V3D work (GPU MMU, job queues and binner overflow, the ISR — an estimated 3,300–4,300
lines) is hardware-in-the-loop only: QEMU models no V3D anywhere. It is staged behind two cheap
milestones: reading `V3D_HUB_IDENT0` back as ASCII `"V3D"` with tech version 7 (proves discovery,
mapping, power domain and clock in one register read), then a deliberate MMU fault at VA 0 caught by
the ISR (proves page tables, MMU enable and the interrupt before any command list exists).

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

- No USB, Ethernet or GPIO UART: RP1 is behind PCIe, which is not trained. The xHCI driver's DMA
  sync hooks are barrier-only, which would also be wrong on this non-coherent board.
- No Wi-Fi (the on-board SDIO chip is untouched; see [drivers.md](drivers.md#wi-fi)).
- HDMI console, SD speed negotiation and the rewritten CPU enumeration (MPIDR from the DTB, cores
  distinguished by `Aff1` on the A76) await their first hardware boot.
- Single block device: the SD card. `/dev/vdc` (`/data`) fails to mount by design; device index 0 is
  intentionally empty.
- No multi-block SD transfers; no V3D rendering, so no desktop (the GPU-only policy applies here too).
- `deploy-rpi5.sh` writes an ELF and a `kernel=kernel.elf` config (see above).

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
- Regression: the `raspi4b` boot (`run-qemu.sh --raspi4b`) for SDHCI and the mailbox; the aarch64
  `--direct` QEMU boot, the only QEMU path that exercises the entry stub's cache/TCR/PUD code (Limine
  branches around it). Real-board verification is a serial capture showing the prompt.

## See also

- [README.md](README.md) — targets and hosts
- [kernel/arch-aarch64.md](../kernel/arch-aarch64.md) — exception vectors, GIC, timer, PSCI
- [graphics/mesa.md](../graphics/mesa.md) — the `v3d` Mesa build and its kernel contract
- [servers/filesystems.md](../servers/filesystems.md) — F2FS and the block layer
