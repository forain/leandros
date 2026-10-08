# Virtual machines

Almost all LeandrOS development happens in QEMU, on an x86_64 `q35` machine and an aarch64 `virt`
machine, each with a fixed set of modern virtio devices. This page describes those machines — the
models, the accelerators and their pitfalls, the device set and why each device is configured the way
it is — plus the UTM bundle for macOS and the GPU-capable QEMU build used on Macs. How to *launch* them
(flags, environment variables, networking, display) is in
[getting-started/running.md](../getting-started/running.md).

## Machine models

| Guest | Machine line (from `scripts/run-qemu.sh`) | CPU | Firmware |
|---|---|---|---|
| x86_64 | `-machine q35 -smp 4,sockets=1,cores=2,threads=2` (`q35,i8042=off` with `--no-i8042`) | `-cpu host` under KVM, `-cpu max` under TCG | OVMF (UEFI) → Limine |
| aarch64 | `-machine virt,gic-version=3 -m ${LEANDROS_QEMU_MEM:-4G} -smp 4` | `-cpu host` under HVF/KVM, `-cpu max,lpa2=off` under TCG | AAVMF (UEFI) → Limine |
| aarch64 `--direct` | same `virt` machine, `-kernel` with QEMU's built-in DTB in `x0` | `-cpu max`, TCG | none |
| aarch64 `--raspi4b` | `-machine raspi4b -m 2G -smp 4` (BCM2711, GIC-400) | fixed Cortex-A72 | none (`-kernel`) |

Guest RAM defaults to 4 GiB (`LEANDROS_QEMU_MEM`; `raspi4b` is fixed at 2 GiB). x86_64 SMT is
modelled (two threads per core) so the scheduler's sibling logic and APIC-ID handling see a real
topology. `-cpu host` is required for HVF and KVM passthrough; `-cpu max` is a synthesised TCG-only
model. `lpa2=off` matters: `max` otherwise advertises FEAT_LPA2 (52-bit physical addresses), and
Limine 11.4.1 wedges on it during its final hand-off — spinning on one instruction with the kernel
entry already in `x0`, so the kernel never prints. Nothing in the system uses 52-bit addresses.

Both UEFI paths boot through **Limine** (protocol revision ≥ 6, a project invariant): the runner
builds a FAT32 boot image with Limine, the kernel ELF and the initrd. `--direct` and `--raspi4b` skip
the bootloader; there the initrd is placed with `-device loader` at a fixed address the kernel knows
(`0x48000000` on aarch64, `0x10000000` for the x86_64 direct path). The direct paths always run
under TCG. They matter because `--direct` is the only QEMU path that runs the aarch64 entry stub's
own MMU, cache and page-table setup — Limine branches around it — and that is exactly the code that
real ARM boards run (see [raspberry-pi-5.md](raspberry-pi-5.md)). Boot details are in
[kernel/boot.md](../kernel/boot.md).

## Accelerators

| Host | aarch64 guest | x86_64 guest |
|---|---|---|
| Apple Silicon Mac | **HVF** (UEFI path only) | TCG |
| x86_64 Linux with writable `/dev/kvm` | TCG | **KVM** |
| aarch64 Linux with writable `/dev/kvm` | **KVM** | TCG |

A hypervisor virtualises, it does not translate, so the fast architecture flips with the host. Two
consequences shape how results are read:

- **The accelerator is not the architecture.** A bug that appears "only on x86_64" on a Mac and
  "only on aarch64" on a Linux desktop is a TCG (or a hardware-virtualisation) bug, not an
  architecture bug. Several defects were filed under the wrong architecture name before this was
  understood; report the accelerator alongside the arch. See
  [development/methodology.md](../development/methodology.md).
- **TCG is a different timing world.** A COSMIC session on TCG takes minutes to settle; races that
  never fire under KVM or HVF can fire constantly under TCG and vice versa.

### HVF and the GICv3 port

QEMU 11.1 refuses to start an HVF guest on a GICv2 machine (`HVF does not support GICv2
emulation`). After Homebrew moved to 11.1.1, every aarch64 boot on a Mac died that way for nine days
— silently, with an empty serial log, because the error goes to QEMU's stderr. The kernel was taught
to drive **GICv3 as well as GICv2**, detecting which at `init` (`9063303`, 2026-09-14), and every
launcher now passes `gic-version=3` on every host. The port was small because only four interrupt IDs
are in use on `virt` (two PPIs for timers, one SGI for rescheduling, one SPI); virtio devices are
polled and there is no MSI on aarch64, so no ITS is needed.

Detection traps, measured and not worth retrying:

- `ID_AA64PFR0_EL1.GIC` reads **0 under HVF `-cpu host`** (QEMU masks it because Apple's in-kernel GIC
  bypasses the CPU-interface model), so the first build detected v2 on the very machine the port was
  for.
- `GICD_PIDR2` lives at `0xFE8` on v2 and `0xFFE8` on v3; reading `0xFFE8` on a GICv2 board hits an
  unassigned hole and raises an external abort under TCG.
- What works: write `GICD_CTLR.ARE` with the group enables clear and read it back — RAZ/WI on v2, sticky
  on v3. The kernel logs the evidence (`[GIC] ID_AA64PFR0.GIC=… PIDR2@0xFE8=… GICD_CTLR after ARE
  write=… -> GICv3`).

The GICv2 path is still exercised by Pi builds (GIC-400) and can be forced on QEMU with
`LEANDROS_QEMU_EXTRA="-machine virt,gic-version=2"` under TCG (QEMU merges the duplicate `-machine`;
the last one wins). Details: [kernel/arch-aarch64.md](../kernel/arch-aarch64.md).

Two further HVF-specific hazards:

- **MMIO with writeback addressing.** HVF can only emulate a data abort whose syndrome is valid
  (`ISV=1`); a load or store with base-register writeback produces `ISV=0` and QEMU aborts with
  `Assertion isv failed`. `read_volatile` on a pointer can compile to such an instruction. Drivers
  that touch MMIO on aarch64 therefore use explicit single `ldr`/`str` instructions (see
  [drivers.md](drivers.md#rules-every-driver-follows)).
- **A silent Mac boot**: read QEMU's stderr log first (the runner keeps it), then check the image
  provenance — a `--rpi5` or `--raspi4b` build overwrites the shared aarch64 image and dies on `virt`
  before printing anything.

## The virtio device set

Every PCI launch uses modern virtio devices:

| Device | QEMU option | Kernel driver | Notes |
|---|---|---|---|
| Boot disk | `virtio-blk-pci,…,bootindex=0` | [virtio_blk.rs](../../drivers/src/virtio_blk.rs) | the Limine FAT image, becomes `vda` (its ESP `vda1`) |
| Root disk | `virtio-blk-pci` on `f2fs-data0-<arch>.img` | same | `vdb`, mounted as `/dev/root` |
| Data disk | `virtio-blk-pci` on `f2fs-data1-<arch>.img` | same | `vdc`, mounted at `/data` (`nofail`) |
| GPU | `virtio-gpu-pci`, `virtio-gpu-gl-pci`, `virtio-vga-gl` (x86_64), with `venus=on,blob=on` for Venus | [virtio_gpu.rs](../../drivers/src/virtio_gpu.rs) | see [graphics/virtio-gpu.md](../graphics/virtio-gpu.md) |
| Sound | `virtio-sound-pci,streams=1` + a probed `-audiodev` | [snd.rs](../../drivers/src/snd.rs) | see [audio.md](../audio.md) |
| Network | `virtio-net-pci` + user-mode SLIRP or `socket_vmnet` | [virtio_net.rs](../../drivers/src/virtio_net.rs) | see [servers/networking.md](../servers/networking.md) |
| Entropy | `virtio-rng-pci` (added last so no other device moves slot) | [virtio_rng.rs](../../drivers/src/virtio_rng.rs) | seeds the kernel CSPRNG |
| Keyboard, pointer | `virtio-keyboard-pci`, `virtio-tablet-pci` | [virtio_keyboard.rs](../../drivers/src/virtio_keyboard.rs) | event0 / event1; see [input.md](../input.md) |
| Optional: USB | `qemu-xhci` with `usb-storage`, `usb-kbd`, `usb-mouse`, `usb-tablet` | [drivers/usb](../../drivers/usb/src/lib.rs) | `--usb-stick`, `--usb-input`; see [pc.md](pc.md) |

The F2FS images are created by `build-all.sh` (`f2fs-data1` is a byte copy of `data0`). Delete one to
force a clean rebuild; when regenerating, regenerate **both**, because a dirty `/data` image makes
filesystem tests fail on its own. A boot that was killed rather than powered off leaves a dirty root;
the next mount says so (`checkpoint ver N is NOT a clean unmount`).

### Why `disable-legacy=on`

Transitional (legacy-compatible) virtio-blk devices trigger a **QEMU 10.x deadlock** on the `virt`
machine: their doorbell write is handled inside the MMIO write handler, where the coroutine-based
block layer needs an event loop that cannot run. Modern, non-transitional devices use a different
notification path. virtio-snd is modern-only anyway (QEMU rejects a legacy one), and virtio-rng
follows the same rule. The x86_64 command line keeps transitional disks and network, which q35 handles;
the drivers accept either device ID.

### Polling, not interrupts

The block, network, input, sound and entropy drivers poll their queues and set the PCI command
register's **Interrupt Disable** bit. A device whose ISR status is never read holds its INTx line
asserted forever, and on `virt` the four shared PCIe INTx lines would then storm the virtio-gpu
completion interrupt. Only the GPU takes interrupts (MSI-X on x86_64, a shared INTx line on aarch64). The rationale and the generic interrupt
API are in [drivers.md](drivers.md#interrupts).

### Entropy

`virtio-rng` matters most under HVF, where `-cpu host` exposes no `RNDR` and the boot seed would
otherwise be timing jitter alone. The driver (`b658f86`..`e12c241`) keeps one request in flight with a
200 ms bound; it *adds* to the CPU sources (`RDSEED`/`RDRAND`/`RNDR`), jitter, and any DTB
`/chosen/rng-seed`, and never replaces them. The boot log names every source:
`virtio-rng=64B cpu=none jitter=…` on HVF, `virtio-rng=64B cpu=RDSEED=64B` on x86_64 TCG.
`EFI_RNG_PROTOCOL` is not used: it is a boot service, and Limine has exited boot services before the
kernel runs. Without the device the kernel logs `[RNG] no virtio-rng device` and carries on.

### Power and reset

A guest `poweroff` ends QEMU through ACPI S5 (x86_64) or PSCI `SYSTEM_OFF` (aarch64); a guest reboot
restarts the VM through the firmware, as on real hardware. The UEFI launchers therefore no longer pass
`-no-reboot`; `LEANDROS_NO_REBOOT=1` restores it (then QEMU exits on any reset, including a triple
fault). `raspi4b` has no PSCI and halts.

## UTM on macOS

`scripts/make-utm-vm.sh` builds a UTM bundle (`LeandrOS-aarch64.utm`) that boots the aarch64 UEFI
system under UTM's bundled QEMU with HVF — to login and a full COSMIC session. A `.utm` is a
`config.plist` plus a `Data/` folder of disk images, so the script is the `run-qemu.sh` aarch64 line
re-expressed in UTM's configuration format. Rerun it after every `build-all.sh`; the images are APFS
clones, so refreshing takes a second or two.

```sh
scripts/make-utm-vm.sh [--name NAME] [--dest DIR] [--gl] [--mem MiB] [--cpus N] \
                       [--serial-tcp PORT] [--debug-log]
```

What differs from `run-qemu.sh`, and why:

- **Sandboxing.** UTM and its QEMU helper are App-Sandboxed and can open only images declared in its
  `Drive` list; paths in "Additional Arguments" are useless. Drives use `Interface = None` so UTM emits
  only the `-drive if=none` half, and the script adds its own `virtio-blk-pci,…,disable-legacy=on`
  (UTM's own VirtIO interface omits it, which deadlocks QEMU 10.x as above).
- **Sound.** UTM's own sound line lacks `disable-legacy=on` and fails ("modern-only"), and UTM opens its
  SPICE playback channel only when its sound list is non-empty, so the device uses its own CoreAudio
  audiodev instead.
- **USB is disabled** in the configuration so UTM adds no xHCI controller and USB input devices;
  virtio keyboard and tablet are used as in `run-qemu.sh`. UTM's extra `virtio-serial` and guest-agent
  port are ignored by the kernel.
- **Display.** UTM's EDK2 has no virtio-gpu GOP, so Limine passes no framebuffer and the kernel's
  fallback ("No bootloader framebuffer; trying VirtIO GPU") brings up the console — that fallback is
  load-bearing here. `--gl` opts into `virtio-gpu-gl-pci`, since UTM bundles virglrenderer with ANGLE.
- **Serial.** By default the console goes to UTM's terminal window; `--serial-tcp PORT` makes it a TCP
  server (`wait=on`, so no early output is lost) for scripted use with `nc`.

Traps that cost time: a serial entry in Terminal mode without its font settings makes UTM crash with a
trap on start, which looks like the VM silently stopping (check UTM's crash reports first); UTM caches
`config.plist` while running, so edits need a full quit and reopen.

## GPU-capable QEMU on macOS

Homebrew's QEMU has no virglrenderer and no `egl-headless`, so on a Mac the guest would have no
hardware renderer and — under the GPU-only policy — no desktop. `scripts/mac-qemu-gpu/build.sh`
builds a standalone QEMU from pinned sources: ANGLE (Metal, or Vulkan through MoltenVK with
`--angle-vulkan` for guest GLES 3.1), libepoxy with EGL on macOS, virglrenderer with a FIFO stand-in
for eventfd (without it every fence waited for QEMU's 10 ms poll timer), and a small QEMU patch for
ANGLE's `egl-headless`. `run-qemu.sh` auto-detects the installed prefix (`LEANDROS_QEMU_PREFIX`
overrides it) and then resolves `--gpu auto` to virgl, viewed over VNC. Venus does not work on the Mac
yet. Performance and the rendering paths are covered in
[graphics/zink-and-virgl.md](../graphics/zink-and-virgl.md).

## What a virtual machine cannot tell you

QEMU is the regression harness, not a hardware model. It implements exclusive loads and stores
regardless of cacheability, has no caches under TCG, loads ELF files by entry point, uses its own
memory map and peripheral clocks, stops USB controllers at `ExitBootServices`, and gives dense APIC
IDs. Every one of those hid a real bug (see [raspberry-pi-5.md](raspberry-pi-5.md) and [pc.md](pc.md)).
Treat both architectures green in QEMU as evidence about logic, and get serial output from real
silicon before believing a hardware path works.

## Code and tests

- Launchers: [scripts/run-qemu.sh](../../scripts/run-qemu.sh),
  [scripts/make-utm-vm.sh](../../scripts/make-utm-vm.sh),
  [scripts/mac-qemu-gpu/build.sh](../../scripts/mac-qemu-gpu/build.sh)
- Kernel: [arch/aarch64/src/gic.rs](../../arch/aarch64/src/gic.rs) (GICv2/v3 detection),
  [drivers/src/virtio_rng.rs](../../drivers/src/virtio_rng.rs),
  [sched/src/random.rs](../../sched/src/random.rs),
  [kernel/src/power.rs](../../kernel/src/power.rs)
- The regression suites run on both architectures under each available accelerator; see
  [development/testing.md](../development/testing.md).

## See also

- [getting-started/running.md](../getting-started/running.md) — flags, networking, display and audio
  selection
- [drivers.md](drivers.md) — the drivers behind these devices
- [development/debugging.md](../development/debugging.md) — serial capture, the QEMU monitor, HVF
  hazards
