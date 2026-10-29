# kmodB: Y/M/N kernel drivers for LeandrOS, the pragmatic path (2026-10-29)

Framing: deliver real `menuconfig` Y/M/N soon. Keep both arches and both real
machines (HP EliteBook laptop, Pi 5) bootable at every step. The default
build stays all-Y and byte-identical through all of Stage 1.

## 0. TL;DR decisions

1. **Kconfig**: vendor **kconfiglib** (pure Python, ISC licence, real tristate,
   `menuconfig`/`olddefconfig`/`allnoconfig`/`savedefconfig`). Write no TUI of our own.
   `.config` → `scripts/kconfig/to-cargo.py` → `--features drv-…` plus a module list.
2. **Stage 1 (Y/N) gates existing call sites. It does not build a registry.**
   Each driver gets a cargo feature, `#[cfg]` on its `mod`, and a **facade stub
   module** with the same public API (no-ops or `None`/`Err`) when the feature is off. So:
   - no reordering of the hand-tuned init sequence in `kernel/src/init.rs`;
   - no edits at the ~140 cross-crate call sites (servers/net 45, drm 35, f2fs 15,
     pipewire 14, syscall.rs, pm.rs, power.rs);
   - with all features on, the code is literally unchanged. **The gate is a byte-identical kernel ELF hash.**
   A linker-section initcall table would change ordering and LTO output for no
   Stage-1 benefit. Section tables arrive in Stage 2, and only per bus
   (`.kdrv.pci`, `.kdrv.usb`), where modules need to append at runtime.
3. **Stage 2 (M)**: pick **(c) + the explicit-export half of (a)**: *prelinked, build-id-locked
   modules against a named `kapi_*` export table*.
   - Modules are built in the same build, with the same rustc, target spec and RUSTFLAGS
     as the kernel. They are **fully linked at build time** to a fixed VA slot in a module
     window, resolving kernel symbols from a generated `PROVIDE(kapi_x = 0x…);` script.
   - The kernel loader then needs **no relocation code**: it checks the header and
     build-id, maps the PT_LOADs at the slot with W^X, zeroes .bss and calls `init`.
   - Rust ABI is allowed across the boundary (`#[no_mangle] pub fn`, `&'static dyn`,
     slices) because the build-id lock forbids any mismatch, as with Linux modversions,
     only stricter.
   - **No unload in v1.** `delete_module` returns -EBUSY. Modules register `&'static` ops,
     so unload would be unsound until registrations are revocable handles.
4. Modules ship **in the initrd** at `lib/modules/<build-id>/*.kmod`. Today the initrd
   holds only `bin/init`, and `deploy-kernel` already swaps kernel and initrd together,
   so a kernel and its modules can never drift apart on the laptop. The Pi needs one
   check that kexec passes the initrd.
5. First M proofs, in order: **virtio_rng** (308 lines, both arches in QEMU), then
   **e1000e** (QEMU both arches + laptop I219), then **uvc** (laptop webcam, USB
   class-driver path). Boot-critical drivers and i915 stay Y.

## 1. Inventory

`drivers/` crate: 107 entries in `src/`, ~98k lines. Plus `drivers/usb` (~9.3k:
xhci, hub, hid, msc, uvc), `drivers/wifi`, `drivers/nvidia` (11.7k + libnvrm.a,
already behind the `nvidia` feature), and `arch_x86_64::psmouse`. Logical hardware drivers ≈ 40:

| Group | Drivers | Probe / wiring |
|---|---|---|
| virtio (QEMU) | virtio_blk, virtio_net, virtio_gpu, virtio_keyboard, virtio_rng, snd (virtio-snd) | `blkdev::init`, `netdev::init`, `kms::detect_and_configure`, `init.rs:78`, pipewire `Backend::Virtio` |
| x86 PC/laptop PCI | e1000e, r8169, iwlwifi(+iwl_*, wifi_* stack), i915 (11.4k), nvidia, nvme, hda(+hda_capture), lapinv, acpi_power (EC/battery), thermal, pci_pm | `netdev::init` calls each net driver explicitly; `drivers::i915::run` (init.rs:287); `nvme::init(cmdline)`; `nvidia_rm::init` under `cfg(feature)` |
| input | psmouse (i8042), i2c_hid + dw_i2c + i2c_acpi + hid_ptp, virtio_keyboard, usb hid | init.rs:258, i2c via ACPI |
| USB (`drivers/usb`) | xhci (+isoch), hub, hid, msc/usb_storage, uvc, bt (btusb+bthid), dwc3 (Pi) | `usb_hcd::init`, `bt::usb::start`, `usb_storage::init`, cmdline setters |
| Pi 5 (`feature rpi5`) | brcmstb_pcie, rp1(+usb,gpio,clk,fan), macb, sdhci/blkdev_sdhci, brcmf (SDIO Wi-Fi), v3d/kms_v3d, hdmi_probe, hdmi_audio, bcm2712_dma40/gpio/pinctrl, rpi_mailbox, rpi_misc, rpi_pm, pi5_board | `cfg(all(aarch64, rpi5))` blocks in init.rs:123-334 |
| core (always Y) | pci, irq, virtio transport, netdev, blkdev, drm core + drm_device_interface (11.3k), kms, framebuffer/console, serial, firmware, sync_file/syncobj | infrastructure |

Wiring style: **explicit calls, no registry.** Most drivers take a `cmdline: &str`
knob string (`e1000e::init(cmdline)`, `pcie.rpi5=0`, `pi5.board=0`, `rp1.irq=1`,
`evdev.serialkbd=1`, `nvidia_probe::set_cmdline`). Existing cargo features: only `rpi5`,
`raspi4b`, `usb-debug`, `nvidia`.

How tangled:
- **Leaf, easy**: virtio_rng, e1000e, r8169, macb, nvme, psmouse, uvc, bt, hda. Each has
  one init call plus a `present()` check in netdev or a server.
- **Medium**: iwlwifi/brcmf, which share the `wifi_*` stack (a Kconfig "library" symbol,
  `select WIFI_STACK`); virtio_gpu, which `kms.rs` calls inline; and `snd`/`hda`, which are
  concrete variants of pipewire's `enum Backend`.
- **Hard**: i915. It is referenced from `kernel/src/syscall.rs` (GEM mmap, prime dedup,
  ptecheck), `pm.rs` (PmDev table), `power.rs`, `acpi_power`, `power_sysfs`, and the vfs and
  drm servers. N works via a facade stub. M is not worth it.
- Dependencies: i915/virtio_gpu/v3d/nvidia → drm core + kms; iwlwifi/brcmf/wifi_sim →
  wifi stack → netdev; uvc → usb core + v4l2 (`kernel/src/v4l2.rs`); bt → usb; usb_storage
  → usb + blk; hdmi_audio → hdmi_probe + pipewire.

Size measured today (llvm-nm on `target/final-*/kernel`, text+rodata attributed by
symbol path). LTO inlining moves code into callers, so these are **lower bounds**:

| | .text | i915 | iwl+wifi | drm_dev_if | e1000e | r8169 | nvme | virtio_gpu |
|---|---|---|---|---|---|---|---|---|
| x86_64 | 2.67 MB | 199K | 301K | 96K | 30K | 26K | 32K | 21K |
| aarch64 (QEMU) | 2.56 MB | 175K | 284K | 104K | 29K | 25K | 29K | 22K |

**The aarch64 QEMU kernel carries ≥520 KB (≥20% of .text) of x86-laptop-only
drivers** (i915, iwlwifi, wifi stack, e1000e, r8169). That is the concrete N win.
The x86 QEMU/KVM kernel carries the Pi drivers that compile on x86 in the same way.
Boot-time win: absent-device probes cost a PCI config read each, so expect
**<1%** (a few ms). The lane measures it from serial timestamps rather than promising it.
The real wins are size, attack surface, and per-lane build time.

## 2. Stage 1: Kconfig + Y/N

Files (new): `Kconfig`, `drivers/Kconfig`, `drivers/usb/Kconfig`,
`configs/{x86_64,aarch64,rpi5,laptop}_defconfig`, `scripts/kconfig/` (vendored
`kconfiglib.py`, `menuconfig.py`), `scripts/menuconfig.sh`, `scripts/kconfig/to-cargo.py`.

- Symbols: `ARCH_X86_64`/`ARCH_ARM64` (from `--arch`), `BOARD_RPI5`, `BOARD_RASPI4B`
  (a `choice`, replacing the raw features), `PCI`, `USB`, `DRM`, `NET`, `WIFI_STACK`
  (hidden, selected), then one symbol per driver, e.g.
  `config E1000E  tristate "Intel e1000e"  depends on PCI && NET  default y`.
  In Stage 1, every driver is `bool` except those marked `# m-capable`. `tristate` and
  `depends on MODULES` are switched on only when a driver's Stage-2 lane lands.
  `MODULES` is a bool, default n until L5.
- `to-cargo.py` maps `CONFIG_E1000E=y` to kernel feature `drv-e1000e`, which maps to
  `drivers/e1000e`. `=m` means the feature is off and the name is added to
  `target/kmod-list-$arch`. `=n` means the feature is off.
- `build-all.sh`: if `.config` exists, use it; otherwise use `configs/<target>_defconfig`.
  **The defconfigs equal today's set exactly** (aarch64 QEMU keeps i915 etc.). A trimmed
  `aarch64_virt_min_defconfig` is offered, not made the default. `--rpi5` / `INTEL=1`
  select the defconfig. Old flags keep working.
- Kernel `Cargo.toml`: `default = ["drv-all"]` as a safety net, so a bare `cargo build`
  matches today.
- Gating pattern in `drivers/src/lib.rs`:
  ```rust
  #[cfg(feature = "e1000e")] pub mod e1000e;
  #[cfg(not(feature = "e1000e"))] #[path = "stubs/e1000e.rs"] pub mod e1000e;
  ```
  The stub mirrors only the externally used `pub` items (`init`, `present`, ops statics).
  Drift is caught by CI `allnoconfig` + 8× `randconfig` compile checks (aarch64 and x86_64)
  in `scripts/kconfig-check.sh`.
- Gates (all of them required): (1) defconfig kernel ELF **sha256 identical** to the
  pre-lane build for x86_64, aarch64 and rpi5. If it is not reproducible, compare
  `llvm-objdump -d` text. (2) allnoconfig + randconfig compile. (3) aarch64 QEMU boot in
  the lane, both arches at the merge batch.

## 3. Stage 2: M

### 3.1 Option comparison
- **(a) ELF ET_REL + versioned extern "C" vtable**: sound. But it means a runtime relocator
  for 2 arches (x86_64 PC32/PLT32/32S/64; aarch64 ADR_PREL_PG_HI21/ADD_ABS_LO12/LDST*/CALL26
  + veneers, because BL reaches only ±128 MiB) and a C-ABI wrapper for every Rust type that
  crosses. Weeks of risky code before the first module loads.
- **(b) PIC .so with dynsym**: needs a dynamic linker in-kernel. The target specs are
  `relocation-model: static`, and x86_64 uses `code-model: kernel`, so PIC needs a second
  target spec and GOT/PLT handling. Most cost, no benefit for a single-image OS.
- **(c) Same-build, build-id-locked**: the only one that lets us **move relocation to build
  time**, because the kernel has no KASLR (static model, fixed `KERNEL_OFFSET =
  0xffffffff80000000`) and its export addresses are known after the kernel link.
  **Chosen**, with one rule taken from (a): modules may reference **only named `kapi_*`
  exports**, never mangled kernel internals. Release is `lto = true` (fat), and LTO
  internalises or inlines mangled symbols, so linking against them would be unsound.

### 3.2 Pieces
- **`kmod-abi` crate** (new, `lib/kmod-abi`, **feature-free**, so its layouts are identical in
  both builds): shared types (`KmodInfo`, `NetifOps`, `HwrngOps`, `PciDevRef`, `DmaBuf`,
  `IrqHandle`, id-table types) plus `extern "Rust" { fn kapi_…; }` declarations behind safe
  wrappers. v1 surface, about 40 functions:
  - log
  - alloc (via the module's `#[global_allocator]` forwarding to `kapi_alloc`/`kapi_dealloc`)
  - panic forwarding
  - cmdline lookup
  - PCI: config read/write, BAR ioremap via `mm::kva`, bus-master
  - IRQ: MSI/MSI-X + legacy register
  - DMA coherent alloc/free and phys
  - timers: sleep, monotonic ns
  - workqueue / kthread spawn
  - registration: `kapi_netdev_register`, `kapi_hwrng_register`, `kapi_usb_class_register`,
    `kapi_v4l2_register`
  - `kapi_firmware_load`
- **Kernel exports**: `kernel/src/kmod/exports.rs` holds the `#[no_mangle] pub fn kapi_*`
  definitions, each also referenced from a `#[used] static KSYMTAB` (in section `.ksymtab`,
  `KEEP` in all 4 linker scripts plus the derived rpi5 direct one), so LTO cannot drop them.
  In Y builds the driver crate calls the same `kapi_*` functions, LTO inlines them, and the
  Y and M code paths are the same source.
- **Module crate** (e.g. `drivers/mods/virtio_rng`): `#![no_std]`, `crate-type = ["staticlib"]`.
  It depends only on `kmod-abi`, defines `#[panic_handler]` (forwards) and
  `#[global_allocator]` (forwards), and `kmod_info!{ name, init, ids: [...] }`, which emits
  `.kmod.info` and `.kmod.ids`. Its private copies of core/alloc/compiler_builtins get
  `--gc-sections`'d (expect 10-40 KB per module).
  The same crate is used for Y: the `drivers` crate depends on it behind the feature and
  calls `init()`. The driver source is therefore one crate in both modes.
- **Build (`scripts/kmod-build.sh`, called by build-all after each kernel link)**:
  1. `llvm-nm kernel | grep ' kapi_'` → `target/kmod/<variant>/ksyms.ld` (`PROVIDE(kapi_x = 0x…);`).
  2. `cargo build` the module crates with the kernel's exact target spec, RUSTFLAGS and
     `-Z build-std`.
  3. Assign each module a slot in the **module window** and link:
     `ld.lld -T kmod.ld ksyms.ld --gc-sections --no-undefined -e kmod_entry` at the slot VA.
     Any reference to a non-exported kernel symbol is a **link error**, so the ABI boundary
     is enforced at build time, not at runtime. lld inserts AArch64 range-extension thunks.
  4. Stamp the kernel's GNU build-id (add `--build-id=sha1` to the kernel link-args; it is
     not there today) into the module header.
  5. Emit `modules.alias` (packed PCI vid:did / virtio id / USB class / DT compatible →
     name) and copy everything into the initrd at `lib/modules/<build-id>/`.

  This runs per kernel variant (standard + direct have different addresses, so they get
  different build-ids; both module sets go in the shared initrd, which is tiny).
- **Module window**: VA `[0xffffffff90000000, 0xffffffffa0000000)` (256 MiB, PML4/TTBR1 slot
  511 alongside the kernel image, so it is shared with every user PML4 already). It must be
  inside the top 2 GiB for x86_64 `code-model: kernel`, and within ±4 GiB ADRP on aarch64.
  L3 must verify nothing else is mapped there; the KVA window is at `0xffff_f000…`, so none
  is expected.
- **Loader (`kernel/src/kmod/load.rs`)**: reuse `elf/` / `kernel/src/kexec/elf.rs` PT_LOAD
  parsing. Steps:
  1. Check `e_type == ET_EXEC`, the arch, `.kmod.info` magic and ABI version, the
     **build-id == running kernel's**, and that every PT_LOAD sits within its slot.
  2. Allocate buddy pages, map 4 KiB PTEs: text RX, rodata R, data/bss RW+NX. Zero bss.
     I-cache maintenance on aarch64 (`dc cvau`/`ic ivau`).
  3. Call `init(&KmodCtx) -> Result`. On `Err`, the module stays mapped but marked
     `failed` (leaking pages is safer than unmapping something that may have registered
     a pointer). A panic is a kernel panic, exactly as with a built-in driver.
- **Loading paths**:
  1. `kmod.autoload` point in `init.rs`, immediately after the place the built-in driver
     would have run (e.g. after `netdev::init`). It walks unclaimed PCI/virtio devices
     (add `pci::claim()`/`claimed()`) against `modules.alias` and reads files with the
     existing initrd cpio lookup.
  2. USB enumerate hook for unclaimed interfaces (L7).
  3. Cmdline `modules.load=a,b`, `modules.blacklist=x`, `kmod=0` (global kill switch:
     inert).
  4. Linux-numbered `finit_module`/`init_module` syscalls (`delete_module` → -EBUSY),
     `/proc/modules`, plus a tiny `kmodctl` (insmod/lsmod) userland tool.
- **Failure is inert in every case**: missing module, missing alias, build-id mismatch or
  init error each produce one `[kmod]` log line and the device stays unbound. Boot continues.

## 4. What converts to M, and what stays Y

| M first (proof) | Why |
|---|---|
| **virtio_rng** | 308 lines, one `init()` + `read()`, testable on both arches with `-device virtio-rng-pci`; needs only a hwrng registration hook |
| **e1000e** | Self-contained PCI NIC; QEMU `-device e1000e` on both arches; real I219 on the laptop. netdev registration is already dynamic (`register(Kind,&'static NetifOps)`, atomic `COUNT`). Risk: net_server/netwatch may snapshot interfaces at init, so L6 must prove late registration (use the same path as hotplug) |
| **uvc** | USB class driver on the laptop webcam; proves the USB class registry + `v4l2` late registration |
| Later M | r8169 (PC), hda+hda_capture (needs a pipewire backend registry replacing `enum Backend`), bt, nvidia (big win, but it links the C `libnvrm.a`; a module could carry it once the ABI is proven), iwlwifi (after `WIFI_STACK` is Y-only and exported) |

**Stay Y** (bool in Kconfig):
- boot or root path: pci, irq, xhci + hub + usb_storage/msc (laptop boots from USB),
  nvme, virtio_blk, sdhci, brcmstb_pcie + rp1 (Pi USB/eth root), macb (pi-remote reaches
  the Pi over it), serial, framebuffer/console, keyboard/psmouse (login), drm core, kms,
  virtio_gpu (console);
- i915 (tangled through syscall/pm/power, and the laptop desktop depends on it);
- v3d/hdmi (Pi bring-up still in flux).

N stays available for all of them via stubs where the board allows.

## 5. Lanes (aarch64 QEMU per lane, both arches per merge batch; small lanes)

| # | Lane | Owns | Tests | Effort |
|---|---|---|---|---|
| L0 | `kconfig` | `Kconfig*`, `configs/`, `scripts/kconfig/`, `scripts/menuconfig.sh`, build-all `.config` plumbing | defconfig ELF sha256 == baseline (x86_64, aarch64, rpi5); `menuconfig` round-trips | 1 d |
| L1a | `kcfg-net` | features+stubs: e1000e, r8169, macb, iwlwifi+wifi stack, brcmf, wifi_sim, virtio_net (`drivers/src/lib.rs` net part, `netdev.rs` cfgs, `stubs/`) | hash gate, allno/randconfig compile, aarch64 boot | 1.5 d |
| L1b | `kcfg-gpu` | i915 (stub facade incl. syscall/pm/power users), nvidia (fold existing feature), v3d, virtio_gpu, hdmi_* | same + laptop boot of defconfig (desktop up) | 2 d |
| L1c | `kcfg-misc` | usb class (uvc, bt, hid), hda/snd, nvme, psmouse, i2c_hid, acpi_power, thermal, lapinv | same | 1.5 d |
| L2 | `kcfg-measure` | `configs/*_min_defconfig`, a size/boot report script | size per config, boot-to-init ms (serial timestamps) on both arches + laptop | 0.5 d |
| L3 | `kmod-core` | `lib/kmod-abi`, `kernel/src/kmod/{exports,load,autoload}.rs`, `.ksymtab` KEEP in 4(+1) linker scripts, `--build-id`, module window, syscalls, `/proc/modules` | ktest: load a built-in test blob, mismatched build-id refused, W^X checked; default kernel still boots (window unused) | 3-4 d |
| L4 | `kmod-build` | `scripts/kmod-build.sh`, `kmod.ld`, ksyms extraction, slot assignment, alias generation, initrd packing, `kmodctl` | `hello` module: build both arches, loads, log line, `lsmod`; `kmod=0` inert | 2 d |
| L5 | `kmod-rng` | `drivers/mods/virtio_rng`, hwrng hook | `CONFIG_VIRTIO_RNG=m`: autoload, `/dev/hwrng` reads on aarch64+x86_64 QEMU; `=y` regress unchanged | 1-1.5 d |
| L6 | `kmod-e1000e` | e1000e → own crate on kmod-abi, PCI/IRQ/DMA exports, late netdev registration | QEMU e1000e both arches (`nettest`), **laptop**: `=m` kernel via deploy-kernel (wired eth up, SSH over wifi as fallback), then promote only the `=y` default | 3 d |
| L7 | `kmod-uvc` | usb class-driver registry (built-in table + runtime list), v4l2 late register, uvc crate | QEMU fake cam both arches; laptop webcam | 3 d |
| L8 | `kmod-pi` | rpi5 defconfig M check (virtio n/a): load `hello` + `e1000e`-less module set via pi-remote kexec (confirm initrd handover) | Pi boots, `lsmod` | 0.5 d |

Order: L0 → (L1a ∥ L1b ∥ L1c; they share only `drivers/src/lib.rs`, so split it into
per-group `#[path]` include files in L0 to avoid conflicts) → L2. Then L3 → L4 → L5 →
(L6 ∥ L7) → L8.

Stage 1 (real Y/N in menuconfig) ≈ **1 week**. Stage 2 to the first module ≈ **1.5 weeks
more**; with e1000e + uvc + Pi ≈ **3 weeks total**.

Invariants for every lane:
- With no `.config`, defconfig = all-Y = today. Stage 1 requires a byte-identical hash.
  Stage 2 lanes change code only in drivers that are being converted, and the regress
  20/20 must stay green.
- `MODULES` defaults to n. With `kmod=0`, or with no `lib/modules/<id>` in the initrd,
  the loader does nothing.
- Laptop and Pi are only ever promoted with the default (all-Y) kernel until L6/L8 pass.

## 6. Risks / open checks
- Kernel reproducibility. If two clean builds already differ, the L0 hash gate falls back
  to comparing `objdump` text plus symbol sizes.
- `#[panic_handler]` symbol naming and `__rust_alloc` shims change across nightlies. The
  module defines its own handler and allocator to stay independent of them. Pin the
  toolchain (already pinned in rust-toolchain.toml); the build-id covers the rest.
- Feature unification. `kmod-abi` must stay feature-free. Driver crates that get both Y
  and M builds must not have features that change public types.
- Late registration in net_server/netwatch, pipewire and drm is unproven. Each M lane
  owns proving it for its subsystem.
- Module window collision on aarch64 rpi5 direct-boot (derived linker script): verify in L3.
