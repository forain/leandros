# lane/openrm — 2026-10-07 — G1: OpenRM feasibility spike (RTX 3070 / GA104)

**Verdict: feasible.** NVIDIA's OS-agnostic resource manager builds freestanding with
clang on the Mac in about 1 minute with NVIDIA's own Makefile left unmodified. It links
into the LeandrOS x86_64 kernel behind `--features nvidia`, and the kernel boots to login
in QEMU. With a debug switch, `rm_init_rm()` runs to completion inside our kernel and
returns `true`. That run needed 43 hand-written callbacks plus 19 in a C glue file; the
other 282 are generated stubs. The default kernel is byte-for-byte identical to `main`.
After rebasing onto `b92798a` (sticklog + nvprobe), main's kernels are x86_64 `e2a5a1f7…` and
aarch64 `ae14f181…`, and this branch's feature-off kernels hash the same. Before the rebase,
on `9ab8b8b`, they were x86_64 `215d2d84…` and aarch64 `483b0fba…`.

## 1. Version + firmware (pinned in `ports/openrm/version.sh`)

| item | value |
|---|---|
| OpenRM tag | **570.86.16**: the same version X547/nvidia-haiku builds on (submodule branch `570.86.16-haiku`, 4 small commits: `NV_HAIKU` defines, `-fpic`, a log newline) |
| source | `github.com/NVIDIA/open-gpu-kernel-modules/archive/refs/tags/570.86.16.tar.gz` sha256 `36c373c2ce695e2b6590f9cddad4a0825e448a5556688ed0be240bf2c9101ad7` (18.6 MB; 108 MB unpacked) |
| GSP firmware source | `download.nvidia.com/XFree86/Linux-x86_64/570.86.16/NVIDIA-Linux-x86_64-570.86.16.run` sha256 `4563ea4bb654247f491005a57c72c676aacd95abe1691d71332cecf25261cbd5` (358 MiB). It is a makeself archive whose payload is a zstd tar; `fetch.sh` pulls the two blobs out and deletes the .run |
| `gsp_ga10x.bin` (GA10x/AD10x, **RTX 3070**) | 63,330,032 B, sha256 `1805a695d3695007761a64d4b98ea121a287574a660b44add2e1bb587d7dbf2f` |
| `gsp_tu10x.bin` (TU10x/TU11x/GA100) | 28,345,432 B, sha256 `53a8b59968ed85a7965187ebcf7a150e4f855ebdcc87947ff99ed0f3a9515b8d` |

Both blobs are byte-identical to the copies nvidia-haiku commits under
`firmware/570.86.16/`. linux-firmware cannot be used: it ships nouveau-renamed GSP images
for other driver versions, and RM refuses a GSP image whose version differs from its own.
Newer 570.x tags exist; the pin follows Haiku, so a working reference port is on the same
code. Bumping the version means updating the tag and all four hashes together.

Nothing NVIDIA ships is committed. Everything goes to `$OPENRM_CACHE`, which defaults to
`~/.cache/leandros/openrm` (outside the repo, shared by all worktrees). If you point it
inside the tree, `.openrm-cache/` is gitignored.

## 2. Build recipe

```sh
ports/openrm/fetch.sh            # source + firmware, sha256-verified (zstd needed)
ports/openrm/build.sh            # -> $OPENRM_CACHE/build-x86_64/libnvrm.a  (~1 min, -j16)
ports/openrm/gen-shim.py         # regenerate drivers/nvidia/src/{stubs,rm_exports}.rs
ports/openrm/build-kernel.sh nvidia                         # kernel + OpenRM
LEANDROS_NVRM_SELFTEST=1 ports/openrm/build-kernel.sh nvidia # + rm_init_rm at boot
ARCH=aarch64 ports/openrm/build-kernel.sh                   # default build, either arch
```

- **`build.sh` runs NVIDIA's `src/nvidia/Makefile` unmodified.** File selection comes from
  `srcs.mk` and libspdm's `openspdm.mk`; the defines (`-DNVRM`, `PORT_*`, `RS_*`, …) and
  the final `ld -r --gc-sections` against `exports_link_command.txt` are exactly what the
  Linux open module ships.
- **Only the toolchain is swapped:**
  - `CC = clang --target=x86_64-unknown-linux-gnu`, Homebrew LLVM 23. The `linux-gnu`
    triple is for `__linux__`/`__unix__`, so `cpuopsys.h` picks NV_LINUX/NV_UNIX like the
    tested build. It also means SysV ABI and ELF.
  - `LD` is rust-lld in GNU mode from the pinned toolchain.
- **The Makefile already passes the flags we need:**
  `-ffreestanding -fno-pic -mcmodel=kernel -mno-red-zone -msoft-float -mno-mmx -mno-sse -mno-sse2 -fno-stack-protector -ffunction-sections -fdata-sections -fno-jump-tables -fcf-protection=branch -O2 --std=gnu11`.
  These match `targets/x86_64-unknown-kernel.json`: code-model kernel, static relocation,
  no red zone, soft-float, no SSE. The kernel is linked at `0xffffffff80100000`, inside
  the ±2 GiB window that `-mcmodel=kernel` requires.
  - The GCC-only flags `-mindirect-branch=thunk-extern` and `-mindirect-branch-register`
    are dropped automatically by `TEST_CC_ARG`, so there are no `__x86_indirect_thunk`
    imports.
  - `-fcf-protection` adds `endbr64` instructions, which execute as NOPs.
- **The output has no compiler-runtime, TLS or float imports.** `memcpy`/`memset` are
  localized inside the object. The import list is exactly the OS interface.
- **`lnv_glue.c` (ours) is compiled with the exact command line of `osapi.c`.** The script
  recovers that command with `make -n -W`, so the flags cannot drift from NVIDIA's.
- **Sizes:**

  | | size |
  |---|---|
  | `libnvrm.a` | **14.4 MB** |
  | `nv-kernel.o` | .text 3.4 MB, .rodata 5.25 MB (includes booter/FWSEC ucode bindata), .data 64 KB, .bss 162 KB |
  | Kernel ELF with `nvidia` | **18.5 MB** (default 8.3 MB) |
  | Cache, all-in after the .run is deleted | ≈ 390 MB: source 108 MB, build dir 81 MB, firmware 91 MB, nvidia-haiku reference clone 168 MB (optional, only used by `gen-shim.py --report`) |

## 3. What RM needs from the OS — 344 symbols

`llvm-nm -u nv-kernel.o` lists 344 symbols. Who provides them today:

| provided by | count | what |
|---|---|---|
| C glue (`ports/openrm/glue/lnv_glue.c`) | 19 | data symbols, the registry table, all variadic printf entry points |
| hand-written Rust (`drivers/nvidia/src/basic.rs`, `interim.rs`) | 43 | real implementations |
| generated stubs (`stubs.rs`) | 282 | log the name once, return a failure value |

`gen-shim.py` takes every stub signature from clang's JSON AST of NVIDIA's own headers,
using NVIDIA's flags. It drops every symbol that the glue or a hand-written module
already defines, so callbacks move out of `stubs.rs` one at a time. The "Haiku" column
counts each symbol in nvidia-haiku as either really implemented or a `panic()` stub, as
detected by regex (approximate). Haiku runs NVK on real cards, so its panic set is
roughly the set of callbacks a working GSP + NVK path never reaches.

| category | n | Haiku impl / panic | LeandrOS facility today | status |
|---|---|---|---|---|
| **Heap, mem/string** (`os_alloc_mem`, `os_mem_*`, `os_string_*`, `os_strtoul`, `os_snprintf`, `os_get_random_bytes`) | 11 | 6 / 5 | `alloc` → `mm::slab` (not IRQ-safe); `sched::random::fill` | **done**, except random |
| **Locks / waits** (spinlock, mutex, rwlock, semaphore, wait queue, `os_is_isr`, `os_semaphore_may_sleep`) | 26 | 26 / 0 | spinlock: `arch_interrupt_save/restore` + atomics (**done**). Sleeping mutex, rwlock, semaphore and wait queue are **MISSING**: only `futex_wait/wake` on kernel addresses and `block_on_poll_*`. "In ISR" query is **MISSING** | interim spinning versions |
| **Time** (`os_get_current_*`, ticks, `os_delay*`, `os_schedule`, nano/RC timers, CPU freq) | 14 | 10 / 4 | `sched::monotonic_ns/realtime_ns`, `arch_tsc_khz`. ms sleep: `block_on_poll_prepare_masked(deadline, POLL_MASK_TIMED_ONLY)`. **hrtimer with callback: MISSING.** Periodic: `register_tick_hook`, 100 Hz, 1 free slot | clock + busy delay done |
| **Work / bottom half** (`os_queue_work_item`, `os_flush_work_queue`, UVM) | 5 | 1 / 3 | `sched::spawn(fn() -> !)` + `try_wake_poll_tagged`. **Work queue: MISSING** | stub |
| **PCI / IO / IRQ** (`os_pci_*`, `os_io_*`, atomics) | 16 | 7 / 9 | `drivers::pci::pci_read/write_config_{8,16,32}`. **No `write_config_8`.** The helpers use port 0xCF8 with `u8` offsets, but x86 ECAM is now reachable via `pci::ecam_va()` (ACPI MCFG, lane/nvprobe), so extended config space is **partial**. No BAR sizing helper (the probe does its own). **MSI, vector allocation and runtime IRQ registration on x86: MISSING** (virtio-gpu hardwires IDT 0x41) | stub |
| **Page / DMA alloc** (`nv_alloc_pages`, `nv_free_pages`, `nv_dma_*`, `os_*page*`, NUMA, cache flush) | 44 | 6 / 37 | `mm::buddy::alloc/alloc_below(order, limit)/free`, `mm::virt_to_phys/phys_to_virt` (HHDM), `drivers/usb/src/dma.rs` DmaBuf. No IOMMU, so DMA addr = phys | stub; ~6 needed |
| **Mappings** (`os_map_kernel_space`, `nv_alloc_kernel_mapping`, user mappings, `os_memcpy_*_user`, PAT) | 21 | 13 / 8 | `mm::paging::map_kernel_device(phys, size, PageFlags{MMIO,NOCACHE,WRITECOMBINE})`. PAT has WC in PA5. **No kernel VA allocator** (HHDM alias only); x86 refuses to split HHDM huge pages; **no range unmap.** `sched::uaccess::copy_{from,to}_user` | stub |
| **Firmware / files** (`nv_get_firmware`, `os_*_file`) | 6 | 2 / 4 | **`request_firmware`: MISSING.** Closest is the initrd: `vfs_server` `find_in_initrd` (private) or `get_file_data_by_path`. f2fs has no kernel open-by-path | stub |
| **Process / caps / file-private / events** | 25 | 20 / 5 | `sched::current_tgid/current_pid/current_euid/exe_path`. File-private and events come with the ioctl plumbing (DRM server) | stub |
| **Platform / ACPI / sysinfo** | 32 | 27 / 5 | mostly constant answers. `active_cpu_count`, `cpu_id`, `framebuffer::get_hardware_fb_info` (no bpp, 32 assumed). **No ACPI `_DSM`/`_ROM`** (`NOT_SUPPORTED`, as on Haiku) | 2 done |
| **Power management** | 10 | 1 / 9 | none needed: return "not available" | stub |
| **I2C** | 2 | 2 / 0 | return NULL (Haiku does the same) | stub |
| **Logging / debug** | 10 | 8 / 2 | `drivers::pci::serial_debug` (lock-free) | **done** |
| **Data / registry** (`os_page_*`, `os_cc_*`, `nv_parms`, `os_registry_init`) | 10 | — | C glue; `os_registry_init` pushes the `nv-reg.h` defaults like Linux | **done** |
| **NVLink / NVSwitch OS layer** | 52 | 9 / 39 | not needed for a single GeForce: trivial libc wrappers + NOT_SUPPORTED | stub |
| **libspdm crypto** | 60 | 0 / 60 | only for Confidential Computing (Hopper); never reached on GA104 | stub (permanent) |

**Real work, estimated from Haiku.** Haiku really implements about 140 of the 344, and
about half of those are one-line constant answers. Haiku's whole shim is about 3,000 LOC
of C++, of which `os-haiku.cpp` is 1,900. Everything else is a panic stub on Haiku, and
Haiku runs NVK on real cards, so those callbacks are evidently never reached. One Haiku
bug to avoid: it defines `os_dma_buf_enabled`/`os_imex_channel_is_supported`, which are
**data** symbols, as panic *functions*, so RM reads code bytes as an NvBool. Our glue
defines them as data.

**Kernel prerequisites, in G2 priority order:**
1. **Sleeping mutex, rwlock, semaphore and wait queue callable from C.** RM holds its API
   lock across GSP RPCs that wait milliseconds. Interim spinning works for the selftest,
   but not on a live GPU.
2. **Contiguous DMA, including below-4G.** `buddy::alloc_below` exists. RM's GSP boot needs
   large contiguous buffers: the radix3 page table for the 63 MB ELF, WPR metadata, and
   the libos log and heap buffers. Use `nv_alloc_pages(contiguous=…)`, with `pte_array`
   given per page when not contiguous.
3. **MMIO mapping with UC/WC** for BAR0 (16 MiB regs) and the BAR1/BAR3 apertures, plus a
   kernel VA range allocator if HHDM huge pages block `map_kernel_device`.
4. **PCIe extended config space on x86: AVAILABLE (partial).** `pci::ecam_va()` reads ACPI
   MCFG; it arrived on main with lane/nvprobe. `os_pci_*` still needs to route offsets
   above 0xFF through it, and the `pci_read/write_config_*` helpers still take a `u8`
   offset over port 0xCF8. RM reads capabilities above 0xFF.
5. **MSI for a generic PCI function on x86:** a vector allocator plus a registration API.
   `rm_isr` must be callable with try-lock-only semantics.
6. **A deferred-work thread** for `rm_isr_bh` and `os_queue_work_item`: `sched::spawn` +
   `block_on_poll` + `try_wake_poll_tagged`.
7. **`nv_get_firmware`:** easiest is packing `gsp_ga10x.bin` (63 MB) into the f2fs data
   image or the initrd and exposing a kernel-side read-by-path.
8. **An IRQ-safe allocator for `os_alloc_mem` from `rm_isr`.** Haiku keeps an
   IntrSafePool.
9. **Stack.** Linux runs RM on 16 KiB stacks, so our 128 KiB is ample. But IRQs run on the
   interrupted task's stack (no IST), and there is no guard page. C frames are not covered
   by `check-stack-frames.py`, because clang emits no `.stack_sizes`; add
   `-fstack-size-section` in G2.

## 4. Shim status

- **`drivers/nvidia` is a new crate, `nvidia-rm`.** It is an optional x86_64-only
  dependency of `kernel`, enabled by feature `nvidia = ["dep:nvidia-rm"]`. Its `build.rs`
  links `static=nvrm` from `$NVRM_LIB_DIR` or the cache, and panics with a clear message
  if the archive is missing.
- **`rm_exports.rs` (generated)** keeps all 208 RM entry points from
  `exports_link_command.txt` in a `#[used]` table. That forces the whole RM graph live, so
  any unresolved OS symbol fails the link. Result: **links with zero undefined symbols.**
- **`stubs.rs` (generated)** keeps every stub alive with a `#[used]` fn table; fat LTO is
  on. Each stub logs its name (first 256 calls) and returns `NV_ERR_NOT_SUPPORTED`,
  `-NVL_ERR_NOT_SUPPORTED`, NULL, false or 0.
- **The hook** is one cfg'd line in `kernel/src/init.rs`. It runs right after the read-only
  `nvidia_probe::run` (nvprobe, which always goes first and stays read-only) and
  `klog::init()`, so RM's messages land in the persisted log. It replaced a blank line. It is
  kept on one line on purpose: inserting lines shifts `#[track_caller]`/panic line numbers
  baked into the binary. A 5-line version changed 14 bytes of the default kernel.
- **Default kernel:** byte-identical to `main` on both arches (sha256 above). Both were
  booted to a root login.
- **`--features nvidia`, x86_64 QEMU:** boots to login and logs
  `[nvrm] OpenRM linked; no NVIDIA display controller present`. A shell command runs
  (`RC=0`).
- **`LEANDROS_NVRM_SELFTEST=1` (no GPU):**
  - The first attempt, with pure stubs, ran RM code. It took a NULL `portSyncSpinlockAcquire`
    fault after the failed `os_alloc_spinlock`: RM does not survive failing core callbacks.
    That is why `basic.rs` exists.
  - With `basic` + `interim`: `NVRM: _sysCreateOs: RM Access Sys Cap creation failed: 0x56`
    (harmless; there is no `/dev/nvidia-caps`), then
    **`[nvrm] selftest: rm_init_rm returned true`**, then normal boot to login.
  - Stubs still hit: `os_is_isr`/`os_get_current_thread` (27 each), `os_is_vgx_hyper`,
    `os_is_efi_enabled`, `os_is_nvswitch_present`, `os_get_cpu_frequency`,
    `os_get_cpu_count`, `os_get_current_process[_name]`, `os_check_access`.
- **aarch64 is feature-off only.** OpenRM itself builds for aarch64 with the same Makefile
  (`-mgeneral-regs-only -mstrict-align`, clang `aarch64-unknown-linux-gnu`): 0 errors,
  8.7 MB .text+.rodata, 336 imports. Compared with x86_64 it drops `os_io_*`,
  `os_pat_supported`, `os_get_acpi_rsdp_from_uefi` and `nv_get_updated_emu_seg`, and adds
  `os_get_smbios_header`. Not wired; not a goal.

## 5. Next lane plan — G2: `rm_init_adapter` → GSP INIT_DONE on the RTX 3070

**The probe → GSP-boot call sequence** follows Linux `nv_pci_probe` and `nv_start_device`
and Haiku's `Device.cpp`. `sp` = NULL throughout.
1. `rm_init_rm(NULL)`, once, at driver init. **Works today.** It calls `os_registry_init`
   (glue), which writes the `nv_parms` defaults.
2. Per GPU, `rm::probe_device` (written already):
   - `os_pci_init_handle`
   - allocate `nv_state_t` of `lnv_state_size()` bytes, then `lnv_state_init`
   - BAR sizing, then `lnv_state_set_bar` for REGS = BAR0, FB = BAR1, IMEM = BAR3
   - `rm_init_event_locks`
   - `rm_is_supported_device`
   - `rm_init_private_state`
   - `rm_set_rm_firmware_requested`
3. Start the device (Linux does this on first open; do it eagerly for G2):
   - enable MEM + bus-master in `PCI_COMMAND`
   - read registry `NV_REG_ENABLE_MSI`, set up MSI, install the ISR
   - `rm_init_adapter(NULL, nv)`. This is the GSP boot: it maps BAR0
     (`os_map_kernel_space`), reads the VBIOS/FWSEC, loads `gsp_ga10x.bin` via
     `nv_get_firmware`, builds radix3 + WPR metadata with `nv_alloc_pages`, runs the
     booter, and **polls** for `GSP_INIT_DONE` (`_kgspRpcRecvPoll` handles INIT_DONE, so
     no IRQ is needed to get there).
4. ISR: `rm_isr(NULL, nv, &need_bh)`. If it returns true with `need_bh` set, queue
   `rm_isr_bh(NULL, nv)` onto a kernel thread. Adopt Haiku's registry tweak
   `NV_REG_PROCESS_NONSTALL_INTR_IN_LOCKLESS_ISR = DISABLE` so nonstall work also goes
   through the BH.
5. Teardown: `rm_disable_adapter` → `rm_shutdown_adapter` → `rm_free_private_state` →
   `rm_shutdown_rm`.

**Ordered work list for G2:**
1. **Kernel:** a sleeping `KMutex`/`KSemaphore`/`KRwLock`/`KWaitQueue` built on
   `block_on_poll`/futex, plus an "in IRQ" per-CPU flag. Swap them in for `interim.rs`.
2. **`nv_alloc_pages`/`nv_free_pages`/`nv_alloc_kernel_mapping`/`nv_free_kernel_mapping`
   and `os_map_kernel_space`/`os_unmap_kernel_space`.** Haiku's are 120 LOC. Use
   `buddy::alloc` + HHDM for system memory, and `map_kernel_device` with `PageFlags` for
   cache type.
3. **`os_pci_*` on a real handle** (the `PciDevice` copy). `os_pci_write_byte` needs a new
   `pci_write_config_8`. Route offsets above 0xFF through `pci::ecam_va()`. Reuse nvprobe's
   BAR/ReBAR decode instead of `rm.rs`'s private `size_bar`.
4. **`nv_get_firmware`/`nv_put_firmware`** reading `/lib/firmware/nvidia/570.86.16/gsp_ga10x.bin`.
   Add a kernel read-by-path, or put it in the initrd; 63 MB means reconsidering the
   initrd's size.
5. **The remaining "Haiku impl" constants**: process/thread ids from `sched`, cpu
   count/number, `os_is_efi_enabled = true`, `os_is_vgx_hyper = false`, `os_pat_supported`,
   `nv_requires_dma_remap = false`, `nv_get_dev_minor`, ACPI = NOT_SUPPORTED, i2c = NULL,
   RC timer no-op. About 60 one-liners, mostly copied from `os-haiku.cpp`.
6. **MSI on x86:** a vector allocator and handler registration (a generalisation of
   virtio-gpu's fixed IDT 0x41 hack), plus the BH thread. The GSP boot itself can be
   brought up by polling before this lands.
7. **Hardware run** on the x86 box with the RTX 3070: `--features nvidia` kernel, and
   `NVreg_ResmanDebugLevel` lowered for verbose RM logs. Success criterion: RM logs GSP
   INIT_DONE and `rm_init_adapter` returns true. Expect several iterations, and keep a
   serial console attached.
8. **After INIT_DONE (later lanes):** the `rm_ioctl` path via the DRM server
   (nouveau-UAPI translation for NVK), `os_memcpy_*_user`, user mappings, file-private and
   events.

## 6. Licensing

`COPYING`: *"Except where noted otherwise, the individual files … are licensed as MIT."*
The dual MIT/GPLv2 applies only to the linked Linux module. LeandrOS takes the files
under MIT. `ports/openrm/license-audit.py` covers all 2,650 files compiled into
`nv-kernel.o` (sources from `srcs.mk`/`openspdm.mk`, headers from the compiler `.d`
files):

| files | license |
|---|---|
| 754 sources + 1,344 headers | SPDX `MIT` |
| 3 sources + 47 headers | MIT text without an SPDX tag |
| 192 sources + 209 headers | no per-file notice: generated NVOC/HAL files and thin headers, covered by COPYING (MIT) |
| 41 sources + 1 header (+37 headers) | **libspdm 3.1.1** (DMTF): BSD-3-Clause |
| `nvSha256.c/.h` | **BSD-3-Clause** (Olivier Gay) |
| `src/lib/zlib/inflate.c` | **zlib** (Mark Adler, "not copyrighted") |
| 7 sources + 10 headers | **x86emu** (SciTech, permissive MIT-style) |

No GPL-only files are compiled. Every non-MIT component is permissive and
GPLv3-compatible. A binary distribution of the `nvidia` kernel must carry the MIT, BSD-3,
zlib and SciTech notices. Our own glue (`lnv_glue.[ch]`, `protos_*.c`) is tagged
`SPDX-License-Identifier: MIT` so it can be shared with the RM side. The GSP firmware is
NVIDIA-proprietary but redistributable (that is how linux-firmware and Haiku ship it).
It is never committed; `fetch.sh` pulls it.

## Files

- `ports/openrm/`:
  - `version.sh`: pins
  - `fetch.sh`
  - `build.sh`: libnvrm.a
  - `gen-shim.py`: stub and keep-table generator; `--report` gives the category table
  - `license-audit.py`
  - `build-kernel.sh`: kernel-only build, either arch, optional features
  - `glue/lnv_glue.[ch]`: C glue
  - `glue/protos_{rm,spdm}.c`: AST inputs for the generator
- `drivers/nvidia/`, crate `nvidia-rm`:
  - `lib.rs`: init, probe, logging
  - `rm.rs`: typed rm_* calls and `probe_device`
  - `glue.rs`
  - `basic.rs`, `interim.rs`
  - `stubs.rs`, `rm_exports.rs`: generated
- `kernel/Cargo.toml`: feature `nvidia`. `kernel/src/init.rs`: the one-line hook.
