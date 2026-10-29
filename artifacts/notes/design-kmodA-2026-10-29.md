# kmodA: Linux-style Y/M/N drivers for LeandrOS (design, 2026-10-29)

Framing: a faithful Linux model, Kconfig + menuconfig + `.config` with per-driver
tristate, `.ko` modules with insmod/rmmod/modprobe, modules.dep, modalias autoload,
initramfs modules. Architecture only; no code was changed.

## 0. What the tree looks like today (facts this design rests on)

- One static `no_std` kernel binary (`kernel` bin crate) that depends on every
  driver crate: `drivers` (107 entries in `drivers/src`, ~60k lines; biggest are
  i915 11.4k, drm_device_interface 11.3k, brcmf 5.8k, virtio_gpu 4k, iwlwifi 3.6k+iwl_* ~4k,
  v3d 2.8k, e1000e 2.3k, bt 2.2k, r8169 2k, hda 1.5k, macb 1.9k), `drivers/usb`
  (xhci, hid, msc, uvc), `drivers/wifi` (cfg80211/mac80211), `drivers/nvidia`
  (OpenRM C library `libnvrm.a` behind cargo feature `nvidia`).
- Target specs `targets/{x86_64,aarch64}-unknown-kernel.json`: `relocation-model:
  static`, `dynamic-linking: false`, x86_64 `code-model: kernel` + `+soft-float`
  + no red zone, aarch64 `code-model: small` + `-neon,-fp-armv8` softfloat (the
  2026-08-05 FP/SIMD clobber fix: any code running at EL1 must stay softfloat).
- Release profile: `panic = "abort"`, `lto = true` (fat), `opt-level = "s"`;
  `-Z build-std=core,alloc`; toolchain pinned `nightly-2026-04-16`.
- Linker scripts `linkers/{x86_64,aarch64}.ld`: ONE `PT_LOAD FLAGS(7)` (RWX) image at
  `0xffffffff80000000 + PHYS_OFFSET` (x86_64 ~`0xffffffff80100000`, aarch64
  `0xffffffffc0080000`). No exported symbol table, no initcall sections.
- Driver bring-up is a hand-written sequence in `kernel/src/init.rs` (~720 lines)
  plus `kernel/src/main.rs:1909` (virtio_rng): explicit `drivers::X::init()` /
  `probe_boot()` calls, many gated by cmdline knobs (`pcie.rpi5=0`, `v3d.probe=1`,
  `hdmi.probe=0`, `nvprobe.*`, `i915.*`), some by `#[cfg(feature = "rpi5")]`,
  `nvidia`, `target_arch`. Pi probes are bracketed by SError checkpoints.
- Cross-driver coupling is direct Rust calls: `netdev::init` calls
  `e1000e::init` and builds a `NetifOps` table of Rust-ABI `fn` pointers;
  `kernel/src/pm.rs` has a static `PmDev` array naming `drivers::i915::pm_suspend`,
  `drivers::e1000e::pm_suspend`; kexec `devreset.rs`/`dma_shutdown.rs` reach into
  drivers too. There are ~64 `#[no_mangle]` seams and 52 `extern "C"` blocks already
  (precedent for a C-ABI boundary; nvidia is a full C-ABI precedent).
- Useful existing pieces: `mm::kva` (ioremap/vmap window at `0xffff_f000_0000_0000`,
  256 GiB, 4 KiB PTEs, shootdowns), x86 `PageFlags::EXECUTE`/NX (EFER.NXE on),
  aarch64 PXN/UXN; `elf` crate loads `ET_EXEC`/`ET_DYN` user images;
  `kernel/src/kexec` has its own ELF parser; `kernel/src/firmware.rs`
  (`request_firmware`, reads a file from the f2fs root from kernel context);
  `sched::kthread::{spawn, should_stop, join, join_timeout}`; `irq::{request_irq,
  request_msi, free_irq}`; `vendor/spin` patched with a held-lock hook
  (`spin::held::set_hook`, installed by `sched::spinheld`).
- Deploy reality: `laptop-deploy-kernel` and `pi-remote` swap kernel + initrd
  only; kexec carries kernel + initrd. Anything in `/lib/modules` on the root fs
  goes stale on every deploy. This drives the version and initramfs decisions.

A per-driver e1000e census: it imports ~15 kernel items (`mm::phys_to_virt`,
`mm::buddy::alloc_below`, `mm::kva::CacheMode`, `sched::{monotonic_ns,
kthread::{spawn,msleep,udelay}}`, `irq::{request_irq, request_msi, IrqSource,
IrqReturn}`, `pci::*`, `netdev::{LinkMon, kick, kick_from_irq}`) plus `spin`,
`alloc`. That is the realistic size of a leaf driver's kernel API.

## 1. Key decisions (summary)

| # | Decision | Why |
|---|----------|-----|
| D1 | Kconfig via vendored **Kconfiglib** (pure Python, ISC; ships `menuconfig.py` curses TUI, `oldconfig`, `savedefconfig`) | Real Kconfig syntax and tristate semantics (`depends on`, `select`, "Y may not depend on M") for free; runs on macOS with stock python3; no ncurses C build. |
| D2 | `.config` drives **cargo features** for Y, a **module list** for M; N = crate not built, feature off | Keeps `#[cfg]` gating native; the default config reproduces today's kernel. |
| D3 | **One source, two builds**: each modularizable driver becomes its own crate under `modules/<name>/` that depends ONLY on `kapi` (+ core/alloc/spin/bitflags/its own `*_hw` pure-logic code). Y links it into the kernel as an rlib; M builds it as a `.ko`. | Exactly Linux's "same .c, =y or =m". Forces the boundary to exist even for Y, so M is just a link mode. |
| D4 | Module ABI = **`extern "C"` + `#[repr(C)]` stable driver API (`kapi`) with per-symbol modversions CRCs**, plus a coarse `vermagic` (arch, kapi major, target-spec ABI flags, rustc version). NOT a whole-kernel build-id lock. | Rust has no stable ABI, and a module that links any kernel crate gets duplicate statics. A build-id lock would invalidate every module on every `deploy-kernel`. CRCs let unchanged API survive rebuilds and reject exactly the symbols whose signature/type changed. |
| D5 | `.ko` format = **ELF `ET_DYN` shared object, PIC, `-z now`, `-Bsymbolic`, version script exporting only `__kmod_*` + `kexport` entries**. Not `ET_REL`. | Only 4 dynamic reloc types per arch, no ±2 GiB / ±128 MiB range problems (GOT/PLT carry absolute addresses), segments already grouped by permission so W^X is trivial, and `elf` crate already parses ET_DYN. Deviation from Linux (which uses ET_REL) accepted for robustness. |
| D6 | Modules are loaded into a dedicated **module window** carved from `mm::kva` (e.g. top 4 GiB of the KVA window), mapped RW to load, then text RX / rodata+relro R / data+bss RW-NX; aarch64 `dc cvau`/`ic ivau` maintenance. | W^X for modules from day one (the core kernel stays RWX for now; separate lane). |
| D7 | Each module carries its own `core`/`alloc`/`compiler_builtins`/`spin` code; allocator, panic, logging, lock-accounting and every stateful service go through `kapi`. Generics never cross the boundary. | Code duplication is harmless; state duplication is fatal. |
| D8 | Autoload is **kernel-side** (`request_module` -> `kmodd` kthread reading `modules.alias`/`modules.dep`), plus userland `modprobe/insmod/rmmod/lsmod/modinfo` over Linux syscall numbers. | No udev in-guest; boot must autoload root/NIC drivers before userland exists. |
| D9 | **The initrd is the module store** (all `=m` modules + `modules.{dep,alias,order,symvers}` packed by `create_initrd`), `/lib/modules/<kver>` on root is a secondary path. | Kernel+initrd travel together through deploy-kernel, kexec and promote, so module/kernel skew is impossible by construction. |
| D10 | Unload is supported but conservative: refcount + owner tags on every registration + post-exit registry audit; anything that leaks a registration becomes a non-freed zombie (taint), DRM/GPU/root drivers are `permanent`. | Use-after-unmap of module text is the worst failure mode; never free text unless provably unreferenced. |

## 2. Kconfig, menuconfig, .config

Files:
- `Kconfig` (top), `arch/Kconfig` (`ARCH_X86_64`, `ARCH_AARCH64`, `BOARD_RPI5`,
  `BOARD_RASPI4B`, `PLATFORM_INTEL` for `INTEL=1`), `drivers/Kconfig` sourcing
  `drivers/net/Kconfig`, `drivers/wifi/Kconfig`, `drivers/gpu/Kconfig`,
  `drivers/sound/Kconfig`, `drivers/usb/Kconfig`, `drivers/bt/Kconfig`,
  `drivers/block/Kconfig`, `drivers/rpi/Kconfig`. Kconfig paths are logical; the
  Kconfig files can all live under `kconfig/` to avoid touching crate dirs.
- `MODULES` bool (top-level "Enable loadable module support"); with it off every
  `tristate` collapses to bool (Kconfiglib does this natively via `modules`
  option on the `MODULES` symbol).
- Existing cargo features become symbols: `rpi5` -> `BOARD_RPI5`, `raspi4b`,
  `nvidia` -> `DRM_NVIDIA` (tristate, x86_64), `usb-debug` -> `USB_DEBUG`.
- Example:

```
config E1000E
    tristate "Intel PRO/1000 PCIe (e1000e, I219)"
    depends on PCI && NET
    default y
    help
      QEMU -device e1000e; HP EliteBook 840 G5 onboard NIC.

config IWLWIFI
    tristate "Intel Wireless WiFi (iwlwifi, 8265)"
    depends on PCI && WIFI_MAC80211 && ARCH_X86_64
    select FW_LOADER
```

- Defconfigs: `configs/x86_64_defconfig`, `configs/aarch64_defconfig`,
  `configs/rpi5_defconfig`, `configs/intel_defconfig`. Initially every driver
  is `y` (identical kernel to today); later stages flip leaf drivers to `m`.
- Tools: `scripts/menuconfig.sh [--arch A] [--rpi5]` runs
  `scripts/kconfig/menuconfig.py` with `KCONFIG_CONFIG=configs/.config-<flavor>`;
  `scripts/kconfig/genconfig.py` emits into `target/kconfig/<flavor>/`:
  - `kernel-features.txt`: cargo features for the kernel build (Y drivers and
    bool options, e.g. `drv-e1000e`), consumed by `build-all.sh` as
    `--features "$(cat ...)"`;
  - `modules.list`: crates to build as `.ko`;
  - `config.rs`: `pub const CONFIG_*` for the rare runtime check;
  - `kconfig.txt`: copy of `.config` embedded in the kernel (`/proc/config`,
    like `IKCONFIG`).
- `build-all.sh` gains `--config FILE` / `--defconfig NAME`; with neither it uses
  the flavor's defconfig, so the existing commands keep working unchanged.

Feature wiring: `kernel/Cargo.toml` gets `drv-e1000e = ["dep:mod-e1000e"]` etc.;
`kernel/src/init.rs` code that names a driver is under `#[cfg(feature = "drv-e1000e")]`
until stage 1 removes the direct calls entirely.

## 3. Driver core (prerequisite for both N and M)

Today init order and cross-calls are hard-wired; M is impossible until the
kernel stops naming drivers. Introduce a minimal Linux-style device model in
`drivers/src/core/`:

- **Initcalls**: `#[link_section = ".initcall.<level>"] #[used] static` entries
  (levels: `core`, `arch`, `subsys`, `fs`, `device`, `late`), linker scripts
  gain `KEEP(*(SORT(.initcall.*)))` with `__initcall_start/__initcall_end`.
  A Y-built module's `module!{}` macro emits its entry here; `init.rs` keeps
  only the boot-critical explicit steps and then runs the device level.
- **Bus types with id tables**: `pci_driver { name, id_table: &[PciId], probe,
  remove, suspend, resume, shutdown }`, `usb_driver` (vendor/product or class
  triplets), `platform_driver` (DT `compatible` strings, ACPI `_HID`). Bus code
  (`pci.rs`, `usb/hcd.rs`, Pi `rp1`/DT) enumerates devices into a device list;
  `driver_register` matches all unbound devices; device add matches all
  drivers; unmatched devices produce a `modalias` and call `request_module`.
- **Registries replace static arrays**: `pm.rs` `PmDev` array -> `pm_register`
  list ordered by probe order (suspend in reverse); kexec `devreset`/
  `dma_shutdown` call `bus.shutdown()` on bound devices; `netdev` keeps its
  registry but takes `extern "C"` ops + owner.
- **Cmdline knobs** become module parameters: `e1000e.foo=1` on the kernel
  cmdline is delivered to a built-in or a later-loaded module identically
  (Linux semantics); `modprobe e1000e foo=1` too. `kapi_param_*` getters.
- Pi SError checkpoints move into the platform bus probe wrapper (checkpoint
  before/after every probe) so ordering-sensitive Pi probes keep their
  diagnostics regardless of who calls them.

## 4. The `kapi` crate (Rust specifics of the boundary)

`lib/kapi` (+ `lib/kapi-macros` proc-macro). Two modes via features:
- `kernel` (linked into the kernel): defines every function, `#[kexport]`.
- `module` (linked into a `.ko`): the same signatures as `extern "C"` imports,
  plus the module runtime (allocator, panic handler, logging macros, `THIS_MODULE`).
- A Y-built driver uses `kernel` mode semantics through direct calls: since the
  definitions are in the same LTO unit, `extern "C"` costs nothing.

### 4.1 Exported symbols and versioning
- `#[kexport] pub extern "C" fn kapi_pci_cfg_read32(dev: KDev, off: u16) -> u32`
  expands to the `#[no_mangle]` fn plus a `#[used] #[link_section = ".kexport"]`
  `KSym { name: *const u8, crc: u32, addr: usize, flags: GPL|UNUSED }`. The
  linker script `KEEP`s `.kexport`/`.kexport_str` and provides `__start/__stop`.
  `#[used]` + `KEEP` defeat fat-LTO dead-stripping of exported functions; no
  `-Z export-executable-symbols` or `.dynsym` needed in the kernel.
- CRC (modversions): `crc32(normalized token stream of the signature)` XOR the
  `KabiHash::HASH` of every parameter/return type. `#[derive(Kabi)]` on
  `#[repr(C)]` types computes a const hash from field names, types and order
  (recursively, at const-eval). Changing a struct's field changes the CRC of every
  function that uses it, exactly like genksyms.
- Kernel build writes `Module.symvers` (name, crc, namespace) by reading the
  `.kexport` section with `llvm-objcopy --dump-section` and a small script.
- Allowed types across the boundary: integers, `bool`, raw pointers, `#[repr(C)]`
  structs/enums with explicit discriminants, `Option<extern "C" fn>`,
  `Option<NonNull<T>>`. Forbidden: `&[T]`, `&str`, `String`, `Vec`, `Box`,
  trait objects, Rust-ABI fn pointers, `Result`. Use `(ptr, len)` and
  `KResult = i32` (negative errno). A `kapi` lint test (`cargo test -p kapi`)
  rejects non-C types via `improper_ctypes_definitions = deny`.
- Opaque handles (`KDev`, `KIrq`, `KNetif`, `KWait`, `KThread`, `KDma`) are
  `#[repr(transparent)] usize` indices or pointers owned by the kernel.

### 4.2 Same-compiler lock: considered and rejected as the primary mechanism
The alternative is to compile modules as rlibs in the same cargo invocation
and let them reference the kernel crates' v0-mangled symbols (stable crate
hashes are identical for one build). It fails here because: (a) fat LTO
internalizes and deletes most non-generic kernel functions, so the kernel would
have to be built without LTO or with a giant keep-list; (b) any `#[inline]` or
generic kernel function instantiated in the module that touches a kernel
`static` silently gets a second copy of that static unless every static is also
exported and referenced by address; (c) `-Zshare-generics` interactions; (d)
every kernel rebuild (each `deploy-kernel`) invalidates every module. It is kept
only as the **vermagic** sanity check: `vermagic = "<kver-base> kapi=<major>
<arch> rustc=<commit-hash> softfloat"` must match or the load is refused; the
CRCs do the fine-grained work.

### 4.3 Allocator, panic, fmt, locks
- **Allocator**: module runtime defines `#[global_allocator] static A: KapiAlloc`
  calling `kapi_kmalloc(size, align, gfp)`/`kapi_kfree(ptr, size, align)`/
  `kapi_krealloc`, which forward to the kernel's `#[global_allocator]`
  (`kernel/src/main.rs:88`). Ownership may cross only as raw pointers with an
  explicit free function, never as `Box`/`Vec`. Debug option
  `CONFIG_KMOD_ALLOC_TRACK` tags allocations with the module id to report leaks
  at rmmod. Page/DMA allocations (`mm::buddy::alloc_below`) are kapi calls
  returning `KDma { phys, virt, len }`.
- **Panic**: `panic = "abort"` in modules (same as kernel). Module
  `#[panic_handler]` formats `PanicInfo` into a 512-byte stack buffer and calls
  `kapi_panic(THIS_MODULE, ptr, len) -> !`, which prints `[kmod e1000e]` and
  enters the normal kernel panic path. No recovery, no unwinding. Kernel panic
  backtraces symbolize module addresses via the module's `.symtab` (kept in the
  `.ko`, registered at load, like kallsyms).
- **fmt**: `core::fmt` is compiled into each module (~20-40 KB). `pr_info!`,
  `pr_warn!`, `dev_info!` format into a stack `ArrayString<256>` and call
  `kapi_log(level, ptr, len)`; klog/serial ordering is the kernel's.
- **spin**: modules carry a copy of `vendor/spin`, whose held-lock hook is a
  per-copy static (`spin::held::set_hook`). Add a `spin` feature `kapi-hook` that
  makes the hook call `kapi_spin_held(+1/-1)` so sleep-while-holding checks and
  `sched::spinheld` accounting stay correct inside modules. Sleeping
  primitives (wait queues, mutexes that block, completions) are kernel objects
  behind handles (`kapi_wait_*`), never module-local.
- **Generics / monomorphisation**: nothing generic crosses. Module-side generic
  helpers wrap C primitives, e.g. `kthread::spawn<F: FnOnce()>` boxes `F`,
  passes `extern "C" fn trampoline::<F>(arg)` + `Box::into_raw` to
  `kapi_kthread_spawn(name, fn, arg, THIS_MODULE)`. `Mutex<T>`, `Vec<T>`,
  `BTreeMap` monomorphise inside the module and touch only module memory.
  Rule enforced by `modpost`: a `.ko` may not define or import any symbol from
  kernel crate namespaces (`_RNv...6kernel`, `...2mm`, `...5sched`, `...7drivers`,
  `...3usb`, `...4wifi`, `...3vfs`); its only undefined symbols must be `kapi_*`
  (or another module's exports). A module's `Cargo.toml` dependency allowlist
  (`kapi`, `spin`, `bitflags`, pure `*_hw` crates) is checked by a script.

## 5. Module build

- New target specs `targets/{x86_64,aarch64}-unknown-kmod.json`: copies of the
  kernel specs with `relocation-model: pic`, `code-model: small` (x86_64 too; PIC
  small is fine anywhere because calls go through GOT/PLT), `dynamic-linking:
  true`, `position-independent-executables: false`, same `features`/`rustc-abi`
  (soft-float on both arches; this is a hard requirement, see the aarch64
  FP/SIMD clobber), `disable-redzone: true`, `frame-pointer: always`, no TLS.
- Each module crate: `crate-type = ["staticlib"]` for M; `rlib` for Y (cargo
  features select). `scripts/build-modules.sh <flavor>`:
  1. `cargo build -p mod-<name> --features kapi/module --target <kmod spec>
     --release -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem`
     (release only, per CLAUDE.md; LTO inside the module is fine).
  2. `rust-lld -flavor gnu -shared -z now -z noexecstack -Bsymbolic --gc-sections
     --version-script=scripts/kmod/kmod.ver -o <name>.ko lib<name>.a`
     (`kmod.ver`: `{ global: __kmod_*; kapi_export_*; local: *; };`).
  3. `scripts/kmod/modpost.py`: check every undefined dynamic symbol is in
     `Module.symvers` (or a dependency module's symvers), write `__versions`
     section (name, crc) and `.modinfo` (`name=`, `vermagic=`, `depends=`,
     `alias=pci:v00008086d000015D7sv*sd*bc*sc*i*`, `license=`, `parm=`) with
     `llvm-objcopy --add-section`; enforce the namespace deny-list; refuse
     `R_*_TPOFF`, `.tdata`, executable stack, writable+executable segments.
  4. `scripts/kmod/depmod.py`: `modules.dep`, `modules.alias`, `modules.order`,
     `modules.symbols`, `modules.builtin` (Y drivers, so `modprobe e1000e` on a Y
     kernel succeeds silently, as on Linux).
- Aliases come from the driver's `id_table` via a `#[derive]`/macro that also
  writes a `.modalias` section, so the id table is the single source.
- `build-all.sh`: after the kernel build, run `build-modules.sh`, then
  `create_initrd` copies `target/modules/<flavor>/` into
  `lib/modules/<kver>/` inside the cpio. Kernel `uname -r` gets a stable
  `<version>-<flavor>`; `vermagic` carries the rest.
- Userland: `userland/kmod-tools` (musl, like the other Rust userland): `insmod`,
  `rmmod`, `lsmod`, `modprobe` (reads `modules.dep`/`.alias`, params,
  `/etc/modprobe.d/*.conf` blacklist/options), `modinfo`, `depmod` (in-guest
  rebuild for `/lib/modules` on root).

## 6. Loader (`kernel/src/module/`)

- Syscalls (Linux numbers): `init_module` (x86_64 175, aarch64 105),
  `finit_module` (313 / 273), `delete_module` (176 / 106); root (CAP_SYS_MODULE)
  only. `/proc/modules`, `/sys/module/<name>/{refcnt,parameters/*,initstate}`.
  Kernel-internal `load_module_from_initrd(name)` / `load_module_from_path`.
- Steps: parse ELF64 `ET_DYN` (reuse/extend the `elf` crate: it already has the
  header/phdr parsing); check `vermagic`, every `__versions` CRC against the
  kexport table (mismatch -> refuse with "disagrees about version of symbol X");
  reserve the module window span (sum of PT_LOAD sizes, 64 KiB aligned, guard
  page after); map RW; copy segments, zero bss; apply relocations; resolve
  imports from kernel `.kexport` then loaded modules' exports (dependency
  refcounts taken here); apply final permissions; cache maintenance; register
  symtab for symbolization; set `THIS_MODULE`; call `__kmod_init` on a kthread
  (so a hung init does not hang the caller forever; timeout logs).
- Relocations (only dynamic ones exist thanks to `-shared -z now -Bsymbolic`):
  - x86_64: `R_X86_64_RELATIVE` (B+A), `R_X86_64_64` (S+A), `R_X86_64_GLOB_DAT`
    (S), `R_X86_64_JUMP_SLOT` (S). Anything else -> refuse.
  - aarch64: `R_AARCH64_RELATIVE` (B+A), `R_AARCH64_ABS64` (S+A),
    `R_AARCH64_GLOB_DAT` (S+A), `R_AARCH64_JUMP_SLOT` (S+A). Anything else ->
    refuse (no TLSDESC, no IRELATIVE).
  - Relocations are applied while pages are RW; `.got`/`.data.rel.ro` then go R.
- Memory: a new `mm::kva::mod_alloc(len) / mod_set_perms(va, len, R|RX|RW)`
  inside a reserved 4 GiB sub-range of the KVA window, 4 KiB PTEs, NX on
  everything but text, never `Global`; perms change triggers the existing
  shootdown. x86_64: the window's PML4 slot already exists before the first user
  process (kva `init`), so user PML4 copies see module mappings.
- aarch64 cache/TLB: after writing text: `dc cvau` per line over text, `dsb ish`,
  `ic ivau` per line, `dsb ish`, `isb`; changing RW->RX: `tlbi vaae1is`, `dsb ish`,
  `isb`. Text pages: AP=EL1 RO, PXN=0, UXN=1. CTR_EL0 line sizes read at runtime
  (the Pi and QEMU differ). x86_64: no cache maintenance; serialize by the
  CR3/INVLPG shootdown already in kva.
- BTI/PAC not enabled in the kernel; leave modules without them (revisit with
  core-kernel hardening).

## 7. Autoload and initramfs

- Every device the bus core cannot bind gets `modalias` (Linux format:
  `pci:vVVVVVVVVdDDDDDDDDsvSSSSSSSSsdSSSSSSSSbcBBscSSiII`,
  `usb:vVVVVpPPPPdDDDDdcDCdscDSdpDPicICiscISipIP`, `of:N*T*C<compatible>`,
  `acpi:<HID>:`) exposed as `/sys/bus/*/devices/*/modalias` and queued to
  `kmodd`.
- `kmodd` kthread: match against `modules.alias` (glob), skip blacklisted,
  resolve `modules.dep` depth-first, load each from the initrd cpio (newc parse
  of the blob the kernel already holds; verify the blob is kept after `/bin/init`
  is extracted, otherwise keep a copy) or from `/lib/modules/<kver>` on the root
  via `kernel/src/firmware.rs` once root is mounted. Loading a driver registers
  its bus driver, which re-matches pending devices -> probe.
- Boot ordering: `kmodd` runs at the `device` initcall level, after PCI/USB/DT
  enumeration; `init.rs` waits for "kmodd idle" (bounded) before mounting root,
  which replaces today's explicit probe calls for anything `=m`. Root drivers
  are either `=y` or listed in `modules.order` priority group "root".
- Userland `modprobe` handles everything after boot (hotplug USB devices still
  autoload through kmodd; userland is optional).

## 8. Unload safety

- `struct KModule { refcnt: AtomicUsize, state: Coming|Live|Going|Zombie,
  deps, users, registrations }`. `try_module_get`/`module_put` are kapi calls.
- Every registration kapi takes `THIS_MODULE` implicitly (the module runtime
  passes it) and records `owner`: bus drivers, IRQ handlers, netdevs, char
  devices (`register_device_notify_open`), PM entries, kthreads, timers,
  procfs/sysfs nodes, DRM/V4L2/ALSA/HCI devices, firmware requests, workqueue
  items.
- Opens take a module ref (`fops.owner`, like Linux): an open `/dev/hci0`,
  `/dev/video0`, `/dev/snd/*` pins the driver module; netdevs pin while up.
- `delete_module(name, O_NONBLOCK)`: refuse if refcnt > 0 or another module
  depends on it (`-EBUSY`, `-EWOULDBLOCK`); state = Going; call `__kmod_exit`;
  the driver must `free_irq`, unregister, `kthread_stop + join`, stop DMA.
  Then the kernel **audits** registries for `owner == m`; any leftover ->
  log `[kmod] e1000e leaked: irq 34, kthread "e1000e-wd"`, state = Zombie, taint,
  **text stays mapped forever** (never free code something may still call).
- In-flight callers: after exit, `kapi_synchronize()` = wait until every CPU
  has passed a scheduling point and every IRQ handler has returned (an IPI
  barrier + per-CPU quiescent counter in `sched`); only then unmap.
- Cannot be unloaded (marked `permanent` at build or refused at runtime): any
  module with no `exit`; GPU/DRM drivers with GEM objects or user mmaps of device
  memory (i915, v3d, virtio_gpu, nvidia) unless all are gone; the driver of the
  root block device and the console; modules others depend on while those are
  loaded; anything with registrations in tables that lack an unregister path.

## 9. What goes M, what stays Y

Must stay Y (boot-critical or core state):
- interrupt controllers and timers (APIC/IOAPIC/HPET/TSC, GICv2/v3, arch timer),
  SMP, `mm`, `sched`, `ipc`, VFS, f2fs, tty/fbcon/framebuffer, serial, klog;
- PCI enumeration + ECAM, ACPI core (and `acpi_power`), DT parsing, IRQ/MSI core,
  DMA core, firmware loader, driver core, kexec devreset core, PM core;
- root storage, unless packed in the initrd and loaded before root mount:
  `virtio_blk` (QEMU), `nvme`, `xhci` + `usb_storage` + hub + HID (the laptop
  and PC boot from a USB stick AND need the USB keyboard at the greeter), `sdhci`
  (Pi SD root), `brcmstb_pcie` + `rp1` (Pi bus fabric);
- DRM core (`drm`, `drm_device_interface`) and the netdev/wifi/sound/v4l2 cores
  that M drivers plug into (`wifi` crate's cfg80211/mac80211 can become an M
  library module later, stage 8).

First M candidates (in order):
1. `kmodtest` (synthetic, exercises every reloc type, statics, alloc, fmt,
   kthread, irq-less timer, params, panic path behind a param) - both arches.
2. `virtio_rng` (~300 lines, QEMU both arches, one call site in `main.rs`).
3. `e1000e` (QEMU `-device e1000e` works on x86_64 q35 AND aarch64 virt PCIe;
   real laptop NIC). The reference leaf network driver.
4. `hda` + `hda_capture` (QEMU `intel-hda`+`hda-duplex` on both arches; laptop).
5. `r8169` (PC; no QEMU model: load/unload test only in QEMU, `-ENODEV` probe).
6. USB class drivers: `uvc` (in `drivers/usb`), `bt` (btusb-like + `bt-host`
   stays Y as the core), `usb_hid` extras beyond boot keyboard.
7. `iwlwifi` + `iwl_*` (needs a C-ABI mac80211 driver interface; hardware only).
8. Pi: `brcmf`, `macb`, `v3d`, `hdmi_audio`, `bcm2712_dma40`, `rpi_misc` (Pi
   hardware; QEMU raspi4b/virt only proves load/unload).
9. `nvidia` (already C ABI via OpenRM: natural `nvidia.ko`, x86_64).
- `i915`: keep Y. 11.4k lines, coupled to DRM, PM/S3, kexec handover, fbcon,
  and the laptop's only display. Possible late `=m permanent` once the DRM kapi
  exists; no value in unloading it.
- `virtio_gpu`: Y (QEMU display), same reasoning.

## 10. Staged lanes (file ownership, tests, effort)

Per the standing rules: lanes test aarch64 only, both arches once per merge
batch on main; release builds only; small lanes. Every stage keeps the
defconfig kernel functionally identical to today until stage 5 flips drivers.

| Stage / lane | Owns | Test (QEMU) | Effort |
|---|---|---|---|
| S0 `kconfig` | `scripts/kconfig/**` (vendored Kconfiglib), `kconfig/**` Kconfig files, `configs/*_defconfig`, `scripts/menuconfig.sh`, `build-all.sh` flag plumbing, `[features]` in `kernel/Cargo.toml`, `drivers/Cargo.toml`, `#[cfg]` gates on call sites in `kernel/src/init.rs` | defconfig boots unchanged (regress); a config with `E1000E=n VIRTIO_RNG=n` builds and boots, `strings` shows no e1000e | 3-4 days |
| S1 `drvcore` | `drivers/src/core/{bus,pci_driver,platform,initcall}.rs`, linker scripts (`.initcall.*`), `pm.rs` registry, `netdev.rs` ops-with-owner, kexec `devreset` via `bus.shutdown`, cmdline -> module params | full regress both arches; S3 + kexec smoke on QEMU; boot log probe order diff | 1.5-2 weeks |
| S2 `kapi` | `lib/kapi`, `lib/kapi-macros`, `.kexport` in linker scripts, `scripts/kmod/symvers.py`, `vendor/spin` `kapi-hook` feature | `cargo test -p kapi` (ctypes lint, CRC stability tests); kernel boots; `Module.symvers` produced | 1 week |
| S3 `kmodload` | `kernel/src/module/**`, `mm/src/kva.rs` (module sub-window + perms), syscall slice in `kernel/src/syscall.rs`, `/proc/modules` + `/sys/module` in `servers/vfs`, symbolizer hook | `modules/kmodtest`: insmod/rmmod x200 loop, every reloc type, W^X probe (write to text faults), CRC mismatch refused, vermagic mismatch refused | 1.5 weeks |
| S4 `kmodbuild` | `targets/*-kmod.json`, `scripts/build-modules.sh`, `scripts/kmod/{modpost,depmod}.py`, `kmod.ver`, `create_initrd` packing, `userland/kmod-tools` | `make`-equivalent produces `.ko` + `modules.*`; initrd contains them; in-guest `lsmod`/`modinfo`/`modprobe kmodtest` | 1 week |
| S5 `kmodfirst` | `modules/virtio-rng`, `modules/e1000e` (moved out of `drivers/src`), their Kconfig entries, defconfig flips to `m` | `-device e1000e` both arches: load, nettest, rmmod, reload, nettest; `VIRTIO_RNG=m` getrandom works | 1 week |
| S6 `kmodauto` | `kmodd`, modalias generation in bus core, USB/DT/ACPI alias formats, initrd cpio reader, root-wait gate in `init.rs` | `E1000E=m`, no manual insmod: NIC up at boot both arches; USB hotplug of a `=m` USB driver autoloads | 1 week |
| S7a `kmodsnd` | `modules/hda` (+ ALSA/pipewire registration kapi) | QEMU intel-hda both arches; laptop audio | 1 week |
| S7b `kmodnet2` | `modules/r8169` | QEMU load/unload; PC later | 3 days |
| S7c `kmodusb` | `modules/uvc`, `modules/btusb` (+ USB driver kapi) | QEMU usb-host-less: load/unload; laptop camera + BT | 1.5 weeks |
| S8 `kmodwifi` | mac80211 driver kapi (in `drivers/wifi`), `modules/iwlwifi` | QEMU load/unload; laptop Wi-Fi WPA2 | 2 weeks |
| S9 `kmodpi` | `modules/{brcmf,macb,v3d,hdmi-audio}`, platform-bus DT aliases | QEMU load/unload; Pi via `pi-remote` | 2 weeks |
| S10 (optional) | `nvidia.ko`; core-kernel W^X (split PT_LOAD into RX/R/RW) | x86 KVM/PC | 1 week |

Totals: infrastructure S0-S6 ~7-8 weeks of lane time; driver waves S7-S9
~6-7 weeks; overall ~3 months. S0 alone already delivers the user's `N`
and keeps `Y` as today; S0-S5 delivers a real `M` for e1000e.

Dependencies: S0 -> S1 -> S2 -> S3 -> S4 -> S5 -> S6; S7a/b/c in parallel after
S6; S8 after S7c (shared USB/registration patterns) or independently after S6;
S9 after S6.

## 11. Risks

1. **API surface explosion** (biggest). Leaf NICs need ~15-30 kapi functions;
   iwlwifi needs mac80211, brcmf needs SDIO + mac80211 + firmware + Pi GPIO,
   i915 would need hundreds. Mitigation: grow kapi per subsystem, keep
   heavy coupled drivers Y, accept that "M" is for leaf drivers first.
2. **Duplicate statics** if a module accidentally links a kernel crate (or an
   `#[inline]` helper reading a kernel static is copied). Mitigation: dependency
   allowlist + modpost namespace deny-list + "only `kapi_*` undefined" rule.
3. **Unload races** (callbacks, kthreads, IRQs, DMA after exit). Mitigation:
   owner tags, post-exit audit, `kapi_synchronize`, zombie instead of unmap,
   `permanent` for GPU/root.
4. **Version skew** across deploy-kernel/kexec/promote. Mitigation: modules in
   the initrd; CRC + vermagic refuse on mismatch with a clear log line; the
   laptop/Pi deploy skills already move kernel+initrd as a pair.
5. **Softfloat / target-flag mismatch** (aarch64 FP/SIMD clobber class): a module
   built with `+neon` would corrupt user `q` registers. Mitigation: `vermagic`
   includes the target-spec ABI flags; modpost rejects objects with FP/SIMD
   instructions on aarch64 (`llvm-objdump` scan for `q`/`v` register use).
6. **Boot ordering regressions** when explicit `init.rs` calls become
   bus-matched or autoloaded (Pi SError checkpoints, USB keyboard before greeter,
   NVIDIA wait). Mitigation: S1 keeps a deterministic `modules.order`; root and
   input stay Y.
7. **Performance**: net RX/TX through GOT/PLT and no cross-boundary LTO. Small
   (one indirect call per kapi call); measure nettest/iperf on e1000e Y vs M.
8. **Code size**: each `.ko` carries its own core/alloc/fmt (est. 60-200 KB per
   module). Fine for the initrd.
9. **Build time**: one extra cargo build per module with `-Z build-std`; share a
   target dir and build modules in one `cargo build -p a -p b ...` to reuse std.
10. **Toolchain pin**: CRCs are toolchain-independent, but vermagic includes the
    rustc commit; bumping `nightly-2026-04-16` rebuilds all modules (they ship
    in the initrd anyway).

## 12. Open questions for the user

- Should `rmmod` be a goal for GPU drivers at all, or is "load-on-demand,
  never unload" acceptable for i915/v3d/nvidia?
- Should `/lib/modules` on root (out-of-initrd modules) exist in v1, or is the
  initrd-only store enough?
- GPL-only export namespaces (Linux `EXPORT_SYMBOL_GPL`) - needed for
  OpenRM/nvidia licensing hygiene, or skip?
