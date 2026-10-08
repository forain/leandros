# Running in QEMU

[`scripts/run-qemu.sh`](../../scripts/run-qemu.sh) boots a built image interactively: the serial console (and QEMU's monitor, multiplexed) on your terminal, the display in a window or a VNC viewer. It probes the host for the fastest accelerator, a working GPU path, an audio backend and a network backend, so the same command works on a Mac, a Linux desktop and a headless build box.

For scripted, headless runs (tests, screenshots, several instances at once) use the driver described in [workflow](../development/workflow.md#the-qemu-driver) instead.

## Usage

```sh
# Test both architectures
./scripts/run-qemu.sh aarch64
./scripts/run-qemu.sh x86_64

# Boot-mode and accelerator overrides
./scripts/run-qemu.sh aarch64 --direct    # DTB via -kernel, no bootloader
./scripts/run-qemu.sh aarch64 --tcg       # force software emulation
./scripts/run-qemu.sh --raspi4b           # BCM2711 board model (aarch64 only)

# GPU path overrides (default: auto)
./scripts/run-qemu.sh x86_64 --venus      # Vulkan: Venus, COSMIC on Zink
./scripts/run-qemu.sh aarch64 --virgl     # OpenGL passthrough
./scripts/run-qemu.sh aarch64 --no-gpu    # plain virtio-gpu: serial login only

# Guest RAM (default 4G; raspi4b is fixed at 2G) — lower it for small hosts
LEANDROS_QEMU_MEM=2G ./scripts/run-qemu.sh aarch64
```

The architecture token can appear anywhere in the argument list; the default is `x86_64`. Unrecognised arguments are passed to QEMU verbatim (`-d <arg>` passes one explicitly).

| Option | Effect |
|---|---|
| `aarch64`, `x86_64` | Guest architecture |
| `--uefi` (default), `--direct`, `--raspi4b` | Boot mode (below) |
| `--hvf`, `--kvm`, `--tcg` | Force an accelerator; an impossible request fails with the reason instead of letting QEMU fail obscurely |
| `--gpu auto\|venus\|virgl\|none`, `--venus`, `--virgl`, `--no-gpu` (alias `--no-virgl`) | GPU path (below) |
| `--venus-headless` | Venus, but always offscreen (`egl-headless`), for harnesses that must not open a window |
| `--usb-stick[=mbr\|gpt]` | Boot from an xHCI USB mass-storage stick holding both the ESP and the root file system, the way real hardware boots; no virtio disks |
| `--usb-input[=kbd,mouse,tablet]` | Keyboard and pointer as USB HID devices on xHCI instead of virtio-input (default `kbd,mouse`) |
| `--no-i8042` | x86-64: remove q35's PS/2 controller, so USB is the only keyboard path |

## Accelerators

`run-qemu.sh` picks the fastest accelerator the host can actually provide for the requested guest:

| Host | Guest | Accelerator |
|---|---|---|
| macOS (Apple Silicon) | aarch64, UEFI boot | HVF |
| Linux, `/dev/kvm` readable and writable | matching architecture | KVM |
| anything else (including cross-architecture) | any | TCG software emulation |

A hypervisor virtualises; it does not translate. A mismatched host/guest architecture therefore always falls back to TCG, which is *much* slower — a COSMIC session on TCG takes minutes to settle rather than seconds. Name results by accelerator, not by architecture: a Mac runs AArch64 fast and x86-64 slow, and a Linux PC the other way round ([methodology](../development/methodology.md#the-accelerator-is-not-the-architecture)).

Machine configuration:

- **AArch64**: `-machine virt,gic-version=3`, 4 CPUs (SMP via PSCI `CPU_ON`), 4 GiB. GICv3 because QEMU 11.1 and later refuse `-accel hvf` on a GICv2 machine ("HVF does not support GICv2 emulation") — a launch refusal, not a guest hang; the kernel detects and drives either GIC. `-cpu host` under HVF/KVM; `-cpu max,lpa2=off` under TCG, because `max` advertises FEAT_LPA2 and Limine 11.4.1 wedges on it in its final hand-off, spinning on one instruction with the kernel entry already in `x0`.
- **x86-64**: `-machine q35`, 4 CPUs as 2 cores × 2 threads (exercising the scheduler's SMT-aware idle-CPU selection), 4 GiB. `-cpu host` under KVM, `-cpu max` under TCG.
- **HVF is only wired to the AArch64 UEFI path.** Direct boot under HVF hangs on an upstream QEMU bug (the PL011 TX FIFO drains on a virtual-time timer that is not serviced while an HVF vCPU runs), so `--direct` and `--raspi4b` always use TCG. See [virtual machines](../hardware/virtual-machines.md).

## Boot modes

| Mode | What boots | Notes |
|---|---|---|
| `--uefi` (default, both architectures) | OVMF/AAVMF → Limine (protocol revision ≥ 6) → `kernel.elf` + initrd from the boot image | The only mode with the full device set and the GPU paths |
| `--direct`, AArch64 | `target/final-aarch64/kernel-direct` via `-kernel`, the `virt` machine's built-in DTB in `x0` | QEMU's `-initrd` is not loaded for a bare ELF, so the initrd is placed at `0x48000000` with `-device loader` and the kernel finds it by scanning RAM for the CPIO `070701` magic. TCG, no network, exits on reset. |
| `--direct`, x86-64 | `target/final-x86_64/kernel-direct-32.elf` via `-kernel` (PVH/Multiboot) | Initrd at `0x10000000` (256 MiB) with `-device loader`. TCG, `-vga none` so virtio-gpu is the only display. |
| `--raspi4b` | The BCM2711 board model with `-machine raspi4b` (fixed 2 GiB) | A stepping stone for the SDHCI driver, not a hardware target: no PCI bus, no GPU, sound or keyboard; the F2FS image attaches through the SD slot; serial only. Build with `--raspi4b` first. |

See [boot](../kernel/boot.md) for what the kernel does in each mode.

## GPU paths

COSMIC renders on the host GPU only. Inside the guest, `/bin/gpu-env` verifies a hardware `GL_RENDERER` (with `/bin/gpuprobe gl`) before any compositor starts; with none, the graphical login is not started, init prints a `NO GPU RENDERER` banner on the console, and only the text logins come up. The runner's job is therefore to attach a GPU device *by default* wherever one can work:

| Mode | Selected by | Device | Guest renders through |
|---|---|---|---|
| `auto` (default) | — | Venus where the host QEMU and virglrenderer can do it (not on macOS), else virgl where GL passthrough exists, else none | — |
| `venus` | `--venus`, `LEANDROS_VENUS=1` | `virtio-vga-gl` (x86-64) or `virtio-gpu-gl-pci` (AArch64) with `venus=on,blob=on,hostmem=` | Zink → Venus → host Vulkan; the device also carries the virgl capset, so the guest can fall back to virgl on it |
| `virgl` | `--virgl`, `LEANDROS_VIRGL=1` | `virtio-vga-gl` (x86-64), `virtio-gpu-gl-pci` (AArch64) | Mesa virgl → host OpenGL |
| `none` | `--no-gpu`, `LEANDROS_GPU=none` | plain `virtio-vga` / `virtio-gpu-pci` | nothing: serial and VT text logins only. Right for headless kernel tests that never draw. |

Every check in `auto` is a capability probe of the QEMU about to run (`-device help`, the `venus=` property, `-display help`, a render node on Linux), never a guess from the OS name. When no GPU path exists the script prints a boxed warning saying why and how to get one.

Details that matter when something does not show up:

- **x86-64 needs a VGA-compatible device**, or OVMF has no GOP framebuffer to hand Limine. `virtio-vga-gl` is virtio-vga plus a virglrenderer context, so it satisfies both; plain `virtio-vga` is the fallback for a QEMU without virglrenderer. With a QEMU that lacks `virtio-vga-gl`, Venus falls back to `virtio-gpu-gl-pci` paired with q35's std-VGA — **two display consoles**, the desktop on the second (View #2, or `screendump -d venusgpu`), the window showing a black console 0.
- **`--venus` never autodetects and never degrades.** A non-GL device, an overridden `-display`, a QEMU without virglrenderer and a macOS host all produce a guest that merely reports "no Venus capset", so `--venus` either produces the proven device line or refuses with the reason. It refuses `-nographic` (which implies `-display none` and silently wins over the display it sets) and `--raspi4b`.
- **Venus display**: a window when the host has a display server, `egl-headless` otherwise. `LEANDROS_VENUS_DISPLAY=<spec>` sets a literal `-display`; `--venus-headless` (or `LEANDROS_VENUS_HEADLESS=1`) forces offscreen.
- **macOS** needs the GPU QEMU from `scripts/mac-qemu-gpu/build.sh` ([prerequisites](prerequisites.md#a-gpu-capable-qemu)). The runner prefers the `--angle-vulkan` install (guest GLES 3.1), then the Metal one (GLES 3.0), then `$PATH`; `LEANDROS_QEMU_PREFIX=<prefix>` picks one explicitly on any host. On a Mac, `auto` resolves to virgl: Venus on macOS is experimental and does not work yet. The ANGLE-on-Vulkan QEMU needs Homebrew's MoltenVK and Vulkan loader, and the runner points `VK_DRIVER_FILES` at MoltenVK's ICD itself and disables ANGLE's VkEvent barriers (`ANGLE_FEATURE_OVERRIDES_DISABLED`), because an unsatisfied VkEvent wait trips the Metal watchdog and after two of those the host refuses all further GPU work from QEMU.
- **AMD hosts**: on a Linux host whose render node is `amdgpu`, the runner exports `GALLIUM_THREAD=0` to QEMU. With radeonsi's threaded context on, a virgl COSMIC session that opened `cosmic-term` faulted the *host* GPU in most boots (page faults at garbage addresses, a ring reset, QEMU exiting); Venus never did. Setting `GALLIUM_THREAD` yourself overrides this.

Inside the guest, software rendering is opt-in for debugging only: `touch /etc/leandros/allow-software-render` (or `LEANDROS_RENDERER=software`). See [graphics](../graphics/README.md), [Zink and virgl](../graphics/zink-and-virgl.md) and [Vulkan/Venus](../graphics/vulkan-venus.md).

## Display

- **A host with a display server** gets QEMU's default UI (`-display default,gl=on` for GL devices).
- **A headless host** (an SSH session with no `$DISPLAY`/`$WAYLAND_DISPLAY`) gets `egl-headless`, which keeps the GPU path alive while rendering offscreen, or `-display none` when GL is unavailable. Without this QEMU's default GTK/SDL backend cannot open and the run dies at startup.
- **macOS with a GPU device**: upstream QEMU's Cocoa UI has no GL, so a GL device always runs under `egl-headless` and is viewed over **VNC** (`LEANDROS_VNC`, default `127.0.0.1:0`, port 5900). Run from a terminal, the script opens the display in macOS Screen Sharing by itself once QEMU's listener is up; Screen Sharing refuses a no-auth VNC server, so an interactive run gets a random one-run password passed in the `vnc://` URL — no prompt. `LEANDROS_NO_VIEWER=1` suppresses the window; non-interactive runs keep a no-auth listener and never open one. `--no-gpu` gives the plain Cocoa window, without COSMIC.
- A **QMP socket** (`/tmp/leandros-qmp-<arch>-<pid>.sock`) is added to every run unless the extra arguments already carry `-qmp`. HMP cannot hold a key chord, so Ctrl+Alt+Fn has no HMP equivalent, and HMP `mouse_move` is relative while the virtio tablet is absolute-only, so it is silently dropped; QMP does both ([debugging](../development/debugging.md#injecting-input)).

## Disks

| Guest device | Host file | Role |
|---|---|---|
| `vda` | `leandros-limine-<arch>.img` | Boot image: ESP with Limine, kernel, initrd |
| `vdb` | `f2fs-data0-<arch>.img` | Root file system (`/dev/root`) |
| `vdc` | `f2fs-data1-<arch>.img` | `/data` |

`build-all.sh` writes all three, data0 populated with the full userland ([building](building.md#what-the-build-does)); they persist across boots. If a data image is missing, the runner creates a blank 64 MB F2FS volume in its place (with `mkfs.f2fs`, or `scripts/mkfs-f2fs-minimal.py` without it) — enough for the kernel to mount, not a usable root. Delete the images and rebuild (or rerun `mkfs-f2fs-populated.py`) to force a clean slate.

Block devices use `disable-legacy=on` (modern, non-transitional virtio) on AArch64: transitional devices trigger a QEMU 10.x deadlock in the doorbell write handler on `virt`, where the coroutine block I/O path needs the iothread event loop that cannot run inside the MMIO handler.

With `--usb-stick`, the stick is the only disk: `scripts/write-usb.sh` builds `leandros-usb-<arch>-<mbr|gpt>.img` (regenerated when an input is newer, sparse) with the ESP on partition 1 and the root F2FS on partition 2 (`root=PARTUUID=…` on the Limine command line); guest writes persist in it until the next regeneration. `LEANDROS_USB_IMAGE=<path>` boots a given image as-is. See [PC hardware](../hardware/pc.md).

## Networking

| Backend | When | Guest address |
|---|---|---|
| `socket_vmnet` (vmnet.framework) | macOS, when its daemon is running | `192.168.105.2`, routable from the host |
| user-mode (SLIRP) | Linux, or a Mac without `socket_vmnet` | `10.0.2.15` behind NAT; gateway and DNS `10.0.2.2` |

Either way the guest configures itself over DHCP with `servers/net`'s smoltcp client, so nothing in the guest needs to know which backend it got; only inbound connections differ (SLIRP needs `-netdev user,hostfwd=…`). If `socket_vmnet` is installed but its daemon is not running, the script says so and falls back to SLIRP instead of failing. `LEANDROS_NET=user` forces SLIRP on a Mac (for a run that needs the `10.0.2.x` layout).

Every QEMU NIC defaults to MAC `52:54:00:12:34:56`. Under `socket_vmnet` all VMs share one bridge, so two concurrent guests would DHCP the same address and reset each other's TCP connections right after the handshake. The runner derives a MAC that is stable per (tree, architecture, `LEANDROS_RUN_ID`); `LEANDROS_MAC` overrides it. See [networking](../servers/networking.md).

## Audio and other devices

QEMU **aborts at startup** if the requested audio backend cannot open, so the backend is probed rather than assumed: CoreAudio on macOS; on Linux, PulseAudio only when a live Pulse/PipeWire-pulse session exists (presence in `-audiodev help` only means it was compiled in), else PipeWire, else ALSA, else `none`. The guest sees one `virtio-sound-pci` stream ([audio](../audio.md)).

Every PCI launch also attaches `virtio-rng-pci`, which seeds the kernel CSPRNG; under HVF, `-cpu host` exposes no `RNDR` and the seed would otherwise be timing jitter alone. Input is `virtio-keyboard-pci` + `virtio-tablet-pci` unless `--usb-input` is given ([input](../input.md)).

A guest `reboot(2)` restarts the VM through the firmware, as on real hardware, and a guest power-off ends QEMU. `LEANDROS_NO_REBOOT=1` restores `-no-reboot` (QEMU exits on any guest reset, a triple fault included).

On macOS the runner holds `caffeinate` for QEMU's lifetime: a MacBook idle-sleeps a few minutes after the last input whatever the CPU load, and a sleeping host freezes the guest, its serial line and its display together, indistinguishably from a guest wedge (the "6–8 minute stalls" of one test wave were exactly that). `LEANDROS_ALLOW_HOST_SLEEP=1` opts out.

Before launching, the runner prints `STALE IMAGE` if the boot image is older than the last commit that changes what goes into it ([building](building.md#image-provenance-pitfalls)).

## Environment variables

| Variable | Effect |
|---|---|
| `LEANDROS_QEMU_MEM` | Guest RAM (default `4G`) |
| `LEANDROS_GPU`, `LEANDROS_VENUS`, `LEANDROS_VIRGL` | GPU path, as the flags |
| `LEANDROS_VENUS_DISPLAY`, `LEANDROS_VENUS_HEADLESS` | Venus display spec / force offscreen |
| `LEANDROS_QEMU_PREFIX` | Use the QEMU (and firmware) under this prefix |
| `LEANDROS_VNC`, `LEANDROS_NO_VIEWER` | macOS VNC listener / suppress the auto-viewer |
| `LEANDROS_NET=user`, `LEANDROS_MAC` | Force SLIRP / set the NIC MAC |
| `LEANDROS_RUN_ID` | Scopes the derived MAC (and the driver's paths) per instance |
| `LEANDROS_USB_IMAGE` | Boot this USB-stick image as-is |
| `LEANDROS_NO_REBOOT=1` | Exit QEMU on any guest reset |
| `LEANDROS_ALLOW_HOST_SLEEP=1` | Do not hold the Mac awake |
| `GALLIUM_THREAD` | Overrides the AMD-host workaround |

## See also

- [Logging in](logging-in.md) — what you see once it boots.
- [Virtual machines](../hardware/virtual-machines.md) — the virtio device set and QEMU machine details.
- [Development workflow](../development/workflow.md) — the headless driver and multi-instance runs.
- [Debugging](../development/debugging.md)
