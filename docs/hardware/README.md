# Hardware

LeandrOS is developed almost entirely under QEMU, on two architectures, but it also boots on real
machines: a Raspberry Pi 5 reaches a shell from its SD card, and an x86_64 UEFI PC boots from a USB
stick with a USB keyboard and brings up NVIDIA's GSP firmware on an RTX 3070. This section describes
which targets exist, how far each one gets, and the drivers behind them.

## Supported targets

| Target | Boot protocol | Status |
|---|---|---|
| x86-64 (QEMU q35) | Limine UEFI | Working |
| AArch64 (QEMU virt) | Limine UEFI (default), or DTB via `-kernel` (`--direct`) | Working |
| AArch64 (QEMU raspi4b) | BCM2711 board model, `--raspi4b` | SDHCI and mailbox test path only |
| Raspberry Pi 5 | RPi firmware flat-image load + BCM2712 DTB (`--rpi5`), or RPi UEFI + Limine | Boot-ready: serial console, F2FS root on the SD card, shell prompt |
| x86-64 real PC (UEFI) | Limine UEFI from a USB stick | Root on the stick, USB keyboard/mouse, console login; NVIDIA GSP-RM initialises, no rendering yet, so no desktop |

Every change must be tested on **both** architectures — cross-platform parity is a project invariant,
not a nice-to-have. QEMU parity is necessary but not sufficient for real hardware: whole classes of
bugs (cache attributes, load addresses, peripheral clocks, firmware-left-running controllers) are
invisible in an emulator by construction. See
[development/methodology.md](../development/methodology.md) and the bring-up records in
[raspberry-pi-5.md](raspberry-pi-5.md) and [pc.md](pc.md).

## Host platforms

Both macOS and Linux hosts build and boot the full system. `run-qemu.sh` picks the fastest
accelerator the host can actually provide for the requested guest:

| Host | Guest | Accelerator |
|---|---|---|
| macOS (Apple Silicon) | aarch64, UEFI boot | HVF |
| Linux, `/dev/kvm` writable | matching arch | KVM |
| anything else (incl. cross-arch) | any | TCG software emulation |

A hypervisor virtualises, it does not translate — so a mismatched host/guest arch always falls back
to TCG, which is *much* slower (a COSMIC session on TCG needs several minutes to settle). Override
with `--hvf`, `--kvm`, or `--tcg`. The accelerator is therefore a property of the *host/guest pair*,
not of the guest architecture: on a Mac aarch64 is fast and x86_64 is emulated, on an x86_64 Linux
box it is the other way round. A bug that shows up "only on x86_64" may really be "only on TCG".
Details of every flag are in [getting-started/running.md](../getting-started/running.md); the
virtual hardware itself is described in [virtual-machines.md](virtual-machines.md).

## Pages in this section

- [raspberry-pi-5.md](raspberry-pi-5.md) — the `rpi5` build, SD-card preparation and deployment, the
  serial console, the six bring-up bugs QEMU could not show, SDHCI, the VideoCore mailbox
  framebuffer, and the V3D groundwork.
- [pc.md](pc.md) — booting a real x86_64 PC: the USB-stick root (xHCI, bulk-only transport, SCSI,
  `/dev/root`), USB keyboards and mice, the on-stick kernel log, CPU enumeration on sparse APIC
  IDs, and the NVIDIA RTX 3070 plan and its GSP-RM milestone.
- [virtual-machines.md](virtual-machines.md) — the QEMU machine models, HVF and the GICv3 port, the
  virtio device set, direct boot, the UTM VM on macOS, and the GPU-capable QEMU build for the Mac.
- [drivers.md](drivers.md) — the `drivers` crate and its satellites (`usb`, `nvidia-rm`, `wifi`):
  PCI, the virtio transports, interrupts, the block layer, firmware loading, and the rules every
  driver follows.

## See also

- [kernel/boot.md](../kernel/boot.md) — Limine (revision ≥ 6), DTB and direct boot, the hand-off to init
- [kernel/arch-aarch64.md](../kernel/arch-aarch64.md), [kernel/arch-x86_64.md](../kernel/arch-x86_64.md)
- [graphics/README.md](../graphics/README.md) — the GPU-only rendering policy that decides whether a
  machine gets a desktop
