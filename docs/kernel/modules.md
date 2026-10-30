# Loadable kernel modules

LeandrOS drivers can be built into the kernel (`=y`), left out (`=n`) or, for a growing list, built as a **loadable module** (`=m`) that lives in the initrd and is linked into the running kernel at boot or on demand. The configuration language is Kconfig, as in Linux; the module machinery is entirely LeandrOS's own: a small ELF loader, a fixed window of kernel address space, and an exported function table (the **kapi**) that is the only way a module reaches the kernel.

> **Status.** Everything here is proven in QEMU on both architectures (and by host-side tests); **nothing has been run as a module on real hardware yet** (laptop and Pi trials are queued). The shipped defconfigs keep every driver `=y` and have `CONFIG_MODULES` unset, so a stock image contains no modules. `=m` is opt-in.

Source map: Kconfig in [Kconfig](../../Kconfig) and [drivers/Kconfig](../../drivers/Kconfig); tooling in [scripts/kconfig/](../../scripts/kconfig) and [scripts/kmod/](../../scripts/kmod); the ABI in [lib/kmod-abi](../../lib/kmod-abi); the module runtime in [lib/kmod-rt](../../lib/kmod-rt) and [lib/kmod-kapi](../../lib/kmod-kapi); the kernel side in [kernel/src/kmod/](../../kernel/src/kmod); the modules in [modules/](../../modules).

## 1. Y / M / N, like Linux

Every driver has a Kconfig symbol. A `bool` symbol is `y` or `n`; a `tristate` symbol may also be `m`, but only when the global `CONFIG_MODULES` is `y`. With `MODULES` unset the build behaves exactly like the old all-built-in kernel.

```sh
./scripts/menuconfig.sh [--arch x86_64|aarch64]          # TUI, edits ./.config
./scripts/menuconfig.sh defconfig <name|path> [--arch A] # .config from configs/<name>_defconfig
./scripts/menuconfig.sh olddefconfig [--arch A]          # fill symbols added since .config was written
```

Defconfigs live in [configs/](../../configs): `x86_64`, `aarch64`, `x86_64_laptop`, `rpi5`. The Kconfig library is vendored (`scripts/kconfig/kconfiglib.py`); [to-cargo.py](../../scripts/kconfig/to-cargo.py) turns `.config` into the two things the build needs: the kernel's Cargo feature list (`drv-*` features) and `target/kconfig-<arch>/kmod-list`, the dash-named crates to build as modules. [build-all.sh](../../scripts/build-all.sh) calls it on every build, so `.config` is the single switchboard; the existing flags (`--rpi5`, `--nvidia`) feed in as symbol overrides.

**What each value produces.**

- `=y`: the driver crate code is compiled into the kernel, as always.
- `=n`: the driver is not compiled. Code elsewhere in the kernel still calls its entry points (`init`, `shutdown_all`, `kexec_disable`, procfs readers), so a **stub** with the same signatures and empty bodies is compiled instead, from [drivers/src/stubs/](../../drivers/src/stubs) (`nvme.rs`, `e1000e.rs`, `usb_storage.rs`, `virtio_net.rs`, ...). The point of a stub is that no `#[cfg]` has to be sprinkled over every caller.
- `=m`: the kernel gets the **stub** too (the kernel must not contain a second copy of the driver), and `kmod-list` gains the module. After the kernel is linked, `scripts/kmod/kmod-build.sh` builds the module crate under `modules/<name>`, links it into a slot, checks it, and stages it into the initrd (section 2). A name in `kmod-list` with no crate fails the build.

Why Kconfig and not ad-hoc cargo features: dependencies (`VIRTIO_NET depends on NET`) are enforced in one place, a menu shows what exists, and a defconfig is a reviewable text diff of what a board ships.

## 2. How a module is built and linked

A module is a Rust `staticlib` in `modules/<dash-name>/` that depends on `kmod-rt` and invokes `kmod_rt::module! { name, init, ids: [...] }` once. Example, [modules/virtio-rng](../../modules/virtio-rng/src/lib.rs):

```rust
mod kenv { pub use kmod_kapi::{mm, pci, sched}; }
#[path = "../../../drivers/src/virtio_rng.rs"]
mod virtio_rng;
kmod_rt::module! { name: "virtio_rng", init: init,
                   ids: [kmod_rt::pci(0x1af4, 0x1044), kmod_rt::pci(0x1af4, 0x1005)] }
```

**The kenv seam.** Driver source must compile both in the kernel and as a module without a fork. Each driver reaches the kernel through a tiny `crate::kenv` module (`drivers/src/kenv.rs`: `pub use ::mm; ::sched; crate::pci`). In the kernel `kenv` re-exports the real crates; in a module crate the line above re-exports `kmod-kapi` instead, a shim with the same function names whose bodies are `extern "C"` calls into the kapi table. The module `#[path]`-includes the *same* driver file. Why: two copies of a driver rot apart, and the shim keeps a driver honest about which kernel services it uses. A driver that still names `crate::pci` directly, or an arch-specific helper, needs a seam added before it can become a module (this is what i915 is waiting for, section 6).

**Fixed slots in a module window.** Modules are not relocated at load time to wherever memory is free. [lib/kmod-abi](../../lib/kmod-abi/src/lib.rs) defines a module window of `0x1000_0000` (256 MiB) at `0xffff_ffff_9000_0000` on x86-64 and `0xffff_fe00_0000_0000` on AArch64, divided into **1024 slots of 256 KiB**. [modules/SLOTS](../../modules/SLOTS) assigns each module `<name> <first_slot> <nslots>` (hello 0/1, virtio_rng 4/4, e1000e 8/16, nvme 32/8, iwlwifi 96/32, ...; slots 1022-1023 are reserved for ktest). The module is **linked at its slot address** by [scripts/kmod/kmod.ld](../../scripts/kmod/kmod.ld), so the image is position-dependent and needs almost no loader work. Why fixed: the symbol addresses of the kernel are also resolved at link time (below), pointers in module data are final, and a slot overlap is a build error (`kmodpost.py lint`) rather than a runtime surprise. A slot only reserves address space; the loader allocates pages as needed.

**Linked against the exact kernel.** After the kernel is linked, `scripts/kmod/ksyms.py` dumps its exported symbol addresses into a linker script, so the module's calls to `kapi_*` are bound directly to the addresses in *that* kernel binary. Both kernels are built with `--build-id=sha1`; the module header carries the id and `kernel/src/kmod/buildid.rs` refuses a module whose id differs from the running kernel's. Staged files live at `lib/modules/<build-id>/<name>.kmod` plus `modules.alias` and `modules.load`; each kernel variant (standard and direct-boot) has its own directory. Consequence: **a module is not portable across builds**, and the kernel and the initrd must always come from the same build.

**Build guards** ([kmodpost.py](../../scripts/kmod/kmodpost.py), failing the build with `kmodpost: <guard>: <reason>`): the SLOTS registry (overlap, range, every module listed); a dependency allowlist (a module may use `kmod-abi`, `kmod-rt`, `kmod-kapi`, `spin`, `bitflags`, ... but not `mm`, `sched`, `drivers`, `usb`, `kernel`, `vfs` or `boot`); undefined symbols must be `kapi_*`; no kernel-crate symbol may be defined in the module; no SIMD/FP instructions (the kernel is built soft-float); W^X segment shape. Code pages are mapped writable only while loading and are published read+execute afterwards.

**kmod-rt** supplies what a `no_std` staticlib lacks: the `module!` macro (header, id table, entry point), a global allocator over the kapi allocator, `kinfo!`-style logging, `panic` forwarding, and id helpers `pci`, `pci_class`, `usb`, and `of`/`sdio` forms. `owner()` returns the loader-assigned id that tags everything the module registers.

## 3. The kapi export table

The kapi is a table of `extern "C"` functions in [kernel/src/kmod/exports/](../../kernel/src/kmod/exports) (one file per group, assembled by `mod.rs`). **It is the only kernel surface a module can reach**; there is no general symbol export as in Linux. The table currently has **`NEXPORTS = 118`** entries. Groups:

| Group | File | Contents |
|---|---|---|
| core / common | `core_.rs`, `common.rs` | log, panic, allocator (`alloc`/`realloc`/`dealloc`), mutex, semaphore, spinlock, preempt, `msleep`/`udelay`, monotonic time, hrtimers, kthreads, `cmdline`, `copy`, `fw` firmware loading, `proc` entries, `ext_event_stats` |
| PCI / mm / hwrng | `pci.rs` | `pci_find`/config access/BAR mapping, `map_device` (flags: writable, MMIO, WC, UC), `ioremap`, DMA allocation, page alloc, `virt_to_phys` (refuses module addresses), `hwrng_register` (also reseeds the CSPRNG) |
| netdev / irq / wait / pm | `net.rs`, `irq.rs`, `pm.rs` | `netdev_register` (wired or Wi-Fi kind), IRQ and MSI-X request, `tick` callbacks, wait queues, power-management hooks ordered by phase |
| sound | `snd.rs` | `snd_register` with `KSndOps` (playback, optional capture and jack service) feeding the PipeWire server |
| block | `blk.rs` | block-device registration for NVMe and virtio-blk |
| Wi-Fi | `wifi.rs` | radio-class registration; the kernel owns the manager, the module is a radio on the table |
| input | `input.rs` | evdev device registration and event injection (7 entries) |
| USB | `usbfn.rs` | interrupt pipes (`usb_intr_open/poll/out/close`), bulk pipes (`usb_bulk_*`), control and isochronous transfers, class registry (19 `usb_*` entries in all) |
| HCI / Bluetooth | `hci.rs` | HCI transport registration plus `usb_bt_open/poll/send/close` |
| V4L2 / UVC | `v4l2.rs` | camera ops table, frame delivery |
| DRM | `drm.rs` | `KDrmOps`, offset, fence and syncobj exports (the class API; no GPU module uses it yet) |

**Registration kinds.** Anything a module hands to the kernel is recorded against its owner id with a `RegKind` ([lib/kmod-abi](../../lib/kmod-abi/src/lib.rs)): `KTHREAD`, `NETDEV`, `HWRNG`, `USB_CLASS`, `V4L2`, `IRQ`, `PCI`, `TICK`, `PM`, `SND`, `BLK`, `INPUT`, `DRM`. `RegKind` is a newtype over `u16`, not an `enum`, so an out-of-range value from a buggy module is an error, not undefined behaviour. **Per-owner release:** if a module's `init` fails, the loader walks that owner's registrations (callbacks, IRQs, tick and PM hooks, devices) and removes them before the slot is freed, so a failed load leaves no dangling function pointer into freed pages. This was a review requirement of the wave and has a ktest for every callback kind.

## 4. Loading

**Syscalls.** `init_module(2)` and `finit_module(2)` are implemented ([kernel/src/kmod/sys.rs](../../kernel/src/kmod/sys.rs)): root only (effective uid 0), image at most 16 MiB, rejected with `EPERM` when `kmod=0`. `delete_module(2)` returns `EBUSY` always; **modules never unload** (section 7). The userland tool [kmodctl](../../userland/kmodctl/kmodctl.c) wraps them: `kmodctl list | load <path> | unload <name> | info <path>` (`unload` therefore fails by design; `info` prints a module's header).

**`/proc/modules`**: one line per module, `<name> <size> <regs> - <Live|Failed|Loading> 0x<base>`, where `regs` counts its registrations.

**Command-line policy** ([autoload.rs](../../kernel/src/kmod/autoload.rs)):

- `kmod=0` disables the window and all loading (and the USB module hook). If the initrd's `modules.load` lists modules anyway, the kernel logs an error instead of silently running without drivers.
- `modules.load=a,b,c` replaces the initrd's `modules.load` list for this boot.
- `modules.blacklist=a,b` refuses those names, both for the early list and for alias autoload.

**`modules.alias` and autoload.** `kmod-build.sh` writes `lib/modules/<build-id>/modules.alias` from each module's id table. Line forms (parsed by `kmod_abi::alias`, `*` = any):

```
pci  <vendor> <device> <module>
pci  <vendor> <device> <class6> <mask6> <module>     # pci-class match
usb  <vid> <pid> <class> <subclass> <proto|*> <module>
of   <compatible> <module>                            # device-tree compatible
sdio <vendor> <device|*> <module>
```

When bus enumeration finds a device that no built-in driver claims, the kernel matches it against the aliases and loads the named module from the initrd (**enumerate-time autoload**): PCI during the scan, USB at device enumeration, `of:` and `sdio:` through the platform registry in `drivers/src/platform.rs`. The loaded module's own probe then binds. Why aliases instead of a hardcoded table: adding a device id is then a module-side change only.

**Initrd modules and the early list.** The initrd carries `lib/modules/<build-id>/*.kmod`. [modules/BOOT](../../modules/BOOT) lists boot-critical modules loaded first, in file order (no sorting): `virtio_blk`, `nvme`, `usb_storage`. `virtio_blk` is first on purpose, because the positional-root rule names the first block device. Only names built `=m` are used; a BOOT name with no crate fails the build (`bidcheck bootcheck`).

## 5. A root disk on a module

The root file system is mounted from a block device, so it looks like a chicken-and-egg problem for a block driver to be a module. It is not, because of how the initrd travels:

- On x86-64 (and the USB-stick image) **Limine** reads the initrd file from the boot partition into RAM as a boot module, next to the kernel. On the Raspberry Pi 5 the firmware does the same: `config.txt` has `initramfs initrd.cpio 0x10000000`.
- The kernel's **built-in cpio reader** unpacks `lib/modules/<build-id>/` from that RAM image (`extract_binary_from_initrd` in [kernel/src/init.rs](../../kernel/src/init.rs)); no driver is needed to read it.
- It loads the BOOT list (`virtio_blk`, `nvme`, `usb_storage`), those drivers register their block devices, and the usual root wait finds the disk.

So NVMe, virtio-blk and USB mass storage can be `=m`, and SDHCI (`modules/sdhci`, in BOOT) as well. Two rules fall out. The kernel and initrd must be from the **same build** (the build-id directory name is the lookup key; a mismatched initrd simply has no matching directory and the root never appears). And the **root wait** is a timing risk: the module path adds load time and, for USB, device enumeration before root is visible; the wait bounds exist but were tuned for built-in drivers.

**Caution, not a technical need:** the shipped configs keep root drivers `=y`. The root-on-module evidence is QEMU only (virtio-blk through `kexecblktest`; NVMe and USB-root `=m` paths are exercised less), and a failure there means a machine that cannot boot, so the default stays conservative until real hardware has done it.

## 6. Which drivers are modules

Tristate symbols and their module (all default to `y`; slot numbers in [modules/SLOTS](../../modules/SLOTS)):

| Kconfig | Module dir | Binds / registers |
|---|---|---|
| `KMOD_HELLO` | `hello` | test module, `of:` alias `leandros,test` |
| `VIRTIO_RNG` | `virtio-rng` | PCI 1af4:1044/1005, `hwrng_register` |
| `VIRTIO_NET` | `virtio-net` | netdev; MSI-X on x86, INTx on AArch64 |
| `VIRTIO_SND` | `virtio-snd` | sound kapi |
| `HDA` | `hda` | Intel HD Audio, playback + capture + jack |
| `E1000E` | `e1000e` | netdev, irq, wait, pm |
| `USB_VIDEO_CLASS` | `uvc` | v4l2 camera, USB ctrl/isoch pipes |
| `NVME` | `nvme` | block kapi, in BOOT |
| `VIRTIO_BLK` | `virtio-blk` | block kapi, first in BOOT |
| `VIRTIO_KEYBOARD` | `virtio-keyboard` | input kapi |
| `IWLWIFI` | `iwlwifi` | Wi-Fi radio class, PCI |
| `USB_HID` | `usb-hid` | boot keyboards/mice via `usb 03 01 01/02` aliases, interrupt pipe, per-device evdev nodes |
| `USB_STORAGE` | `usb-storage` | USB bulk pipes, block kapi, in BOOT |
| `BT` | `bt` | HCI over USB (Intel), `usb_bt_*` pipes |
| `SDHCI` | `sdhci` | SD/eMMC host, block kapi, `of:` aliases `brcm,bcm2712-sdhci` and `brcm,bcm2711-emmc2`, in BOOT; fixed 256 GiB capacity (no C_SIZE) |

**Y-only (bool) drivers:** `VIRTIO_GPU`, `V3D`, `R8169`, `MACB`, `BRCMF`, `HDMI_AUDIO`, `I915`, plus the platform pieces (RP1, bcm2712 blocks, PSMOUSE, I2C-HID, ...). Several already have reserved SLOTS entries (brcmf, i915, virtio_gpu, v3d, macb, hdmi_audio, r8169) but **no module crate**, so they cannot be set to `m`. The DRM class kapi (`KDrmOps`) is merged; i915 is the intended first user but first needs a `kenv` seam over its direct kernel dependencies. `VIRTIO_GPU` and `V3D` were deliberately kept `bool`.

## 7. What a module cannot do, and why

- **Tied to one kernel build.** Build-id lock and addresses linked into the image (section 2). You cannot copy a `.kmod` from another kernel, or update the kernel and keep the old initrd.
- **Slot limits.** A module must fit its SLOTS allocation (e.g. 8 x 256 KiB for nvme). Growing a driver may mean a new SLOTS line (check the file's "next free" comment).
- **Only the kapi is reachable.** A driver that needs a kernel facility not in the 141 exports needs a new export, and an exported function becomes ABI the module crates compile against (`KMOD_ABI_VERSION = 1`).
- **No unload.** `delete_module` is `EBUSY`; a module that registered a thread, IRQ or device cannot be torn down safely. A failed `init` is the only path that releases a slot. For example, with a radio present the iwlwifi radio thread pins the module, so unloading it would only ever be clean with no device.
- **Crash-time and boot-critical paths stay in the kernel.** Panic output, the persistent kernel log, kexec's device quiesce, and the console cannot depend on a loadable driver. Where such a hook is needed from a module (shutdown, suspend, kexec quiesce), it is registered through the kapi power-management table and ordered by phase.
- **Per-driver reductions**, to keep the first versions simple:
  - `usb-storage` is a *lean* variant: no `IoWindow` (atomic-context I/O), no `usbms.*` latency knobs, no hungtask `inflight`/lock-owner reporting, no iostat counters, no background poll for not-yet-ready LUNs (it waits 3 s for ready, then declines the interface). USB root on `=m` is untested.
  - `usb-hid` binds only boot-protocol keyboards and mice, names devices generically (not by product string), and the `=y` HID class path in xhci enumeration is unchanged.
  - `bt` supports the USB transport only; detach is passive (the pump sees `Gone` on its next poll and marks `/dev/hci0` dead); a probe on a device without a complete HCI interface returns `-19` and the kernel keeps the interface.
  - `virtio-net` as a module reports `rx_irq_count` as 0 in `/proc/net` (the counter reads the stub).
  - `iwlwifi` keeps a module-local copy of the Wi-Fi gk counters; `/proc/net/wifi` shows the radio from the registry plus gk lines through netif info only.

## 8. Testing

All of this is in the regression/ktest tooling, none on real hardware:

- **`ktest=kmodkapi`** ([kernel/src/ktest.rs](../../kernel/src/ktest.rs)) exercises the 141 exports and per-callback-kind failed-init release, on both arches.
- **`scripts/kmod/selftest.sh <arch>`** is the module pipeline self-test; **`scripts/kmod/autoloadtest.sh <mod>`** builds a kernel with that driver `=m` and asserts the device works through alias autoload, and that blacklisting it removes the device. Cases: e1000e, virtio_rng, virtio_net (via `nettest`), virtio_keyboard, virtio_snd, hda, iwlwifi, usb_hid, bt, virtio_blk/nvme (negative cases), and `of`.
- **`scripts/kmod/rootmodtest.sh <nvme|usb_storage|sdhci>`** (bus chosen by `LEANDROS_ROOTFS_BUS`) builds a kernel with that storage driver `=m`, boots with the root on that bus and asserts the root mounts through the initrd module path. aarch64 results: nvme PASS, usb_storage PASS, sdhci PASS (`-M raspi4b`). `sdhci` is aarch64 only. The sdhci module reports a fixed 256 GiB capacity because it does not decode C_SIZE, so other card sizes are not reported correctly.
- **`kexecblktest`** (`runtests ... kexecblk`) is the root-on-module gate: a kernel built `MODULES=y VIRTIO_BLK=m` is kexec'd and must mount root.
- **`usbunplugtest.py`** covers USB hot-unplug and not-ready devices, including the `hid` case.
- **`bidcheck.py`** (`scripts/kmod/bidcheck.py build <arch>`, last step of the build) fails the build if a shipped kernel variant has no `lib/modules/<its build-id>/` in the initrd, so a stale or mismatched initrd cannot ship.

## Adding a new module

1. Make the Kconfig symbol `tristate` in [drivers/Kconfig](../../drivers/Kconfig) (default `y`) and add a stub in `drivers/src/stubs/` with the entry points other code calls.
2. Put the driver behind a `kenv` seam so no `use crate::...` reaches past it.
3. Add `modules/<dash-name>/` (a `staticlib`, `kmod_rt::module!`, the driver via `#[path]`, `kenv` re-exporting `kmod_kapi`) and a [modules/SLOTS](../../modules/SLOTS) line (underscore name, first slot, count; keep slots clear of 1022-1023).
4. Add any missing `kapi_*` exports in `kernel/src/kmod/exports/` (and bump `NEXPORTS`), with a `RegKind` if a new registration needs per-owner release.
5. Declare the device ids in the `module!` macro so `modules.alias` is generated; if the module must exist before root, add its name to [modules/BOOT](../../modules/BOOT).
6. Build with the symbol `=m` for both architectures; read the `kmodpost:` guard messages if the build fails.
7. Add an `autoloadtest.sh` case and, if relevant, extend `kmodkapi`; run `bidcheck.py`.
8. Confirm the default (`=y`) kernel still builds and passes regress unchanged.

## See also

- [boot.md](boot.md) — Limine and the initrd hand-off
- [hardware/pc.md](../hardware/pc.md), [hardware/](../hardware/) — drivers that are candidates for modules
- [getting-started/building.md](../getting-started/building.md#kernel-configuration-kconfig) — `menuconfig.sh` and defconfigs
