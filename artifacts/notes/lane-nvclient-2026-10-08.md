# lane/nvclient — 2026-10-08 — G3: an in-kernel RM client that does real GPU work

Branch `lane/nvclient`, base main `7bf2d40`. Not pushed. Mac: x86_64 under TCG,
aarch64 under HVF. Hardware: the RTX 3070 box (GA104 10de:2484, Kaby Lake host),
where lane nvshim brought GSP-RM up (`nvrm-boot1.log`, `rm_init_adapter OK in
3528 ms`).

**Outcome.**
- The kernel has an RM client: the `rm_kernel_rmapi_op` entry point Linux's
  in-kernel clients (nvidia-modeset, UVM's RMAPI) use, wrapped by C glue, with a
  typed Rust API (`drivers/nvidia/src/client.rs`).
- Every parameter block and constant the Rust side uses is checked against
  NVIDIA's own headers at run time: 161 entries match, 0 mismatches.
- QEMU proves the client path end to end with `nvrm.selftest=abi`, no GPU
  needed. It runs `rm_init_rm`, allocates an NV01_ROOT client, makes two NV0000
  controls (driver version `570.86.16`, attached GPU ids), gets the expected
  refusal of a device allocation with RM's own status name, and frees.
- `nvrm.selftest=ce` is the hardware test. After `rm_init_adapter` it runs
  device → subdevice → GPU info → VA space → sysmem + vidmem →
  channel group + channel + copy engine → doorbell submission. The work is
  CE fills and copies with semaphores polled by the CPU, colour bars drawn into
  the GOP console's VRAM, 1000 timed round trips, and a reverse teardown.
  It has not run on the PC yet.
- Two log warnings from boot 1 are fixed: `clCheckUpstreamLtrSupport_IMPL:
  PCIE config space is inaccessible!` and the `clPcieReadWord() failed`
  assertion. RM now gets the ACPI RSDP. QEMU shows the same walk RM does
  finding the MCFG.
- The 5 user-mapping callbacks are implemented (or deliberately answered).
  Stubs are down from 164 to 159.
- Feature-off kernels are byte-identical to main on both arches.

## 1. The RM client API (for the nouveau-UAPI lane)

The stack, from bottom to top:

1. **RM**: `rm_kernel_rmapi_op(sp, nvidia_kernel_rmapi_ops_t *)`
   (`src/nvidia/arch/nvalloc/unix/src/osapi.c`). It has privilege level KERNEL
   and parameter location KERNEL, and takes the RM API and GPU locks inside.
2. **C glue**: `ports/openrm/glue/lnv_rmapi.c` and `lnv_rmapi.h` fill the
   NVOSxx blocks. They are compiled with osapi.c's flags; `build.sh` now
   compiles every glue source.
3. **Rust**: `drivers/nvidia/src/client.rs`, plus `params.rs` for the classes,
   controls, flags and `#[repr(C)]` parameter blocks.

**Calling rule:** task context only (a kernel thread or a syscall). RM sleeps
on its locks and during GSP RPCs. Never call from an ISR or under a spinlock.

```rust
pub type Handle = u32;
pub struct RmError(pub u32);          // NV_STATUS; .name() = RM's nvstatusToString
pub type Result<T> = core::result::Result<T, RmError>;

pub fn abi_check() -> (usize, usize); // (matched, mismatched) vs NVIDIA's headers

pub struct Client;                    // one NV01_ROOT; Drop frees it and everything under it
impl Client {
    pub fn new() -> Result<Client>;                       // RM picks the client handle
    pub fn handle(&self) -> Handle;
    pub fn alloc<P>(&self, parent: Handle, class: u32, params: &mut P) -> Result<Handle>;
    pub fn alloc_noparams(&self, parent: Handle, class: u32) -> Result<Handle>;
    pub fn alloc_raw(&self, parent, class, params: *mut c_void, size: u32) -> Result<Handle>;
    pub fn alloc_with_handle(&self, parent, handle, class, params, size) -> Result<Handle>; // caller-chosen handle (NVK style)
    pub fn control<P>(&self, object: Handle, cmd: u32, params: &mut P) -> Result<()>;
    pub fn control_raw(&self, object, cmd, params: *mut c_void, size: u32) -> Result<()>;
    pub fn free(&self, parent: Handle, object: Handle) -> Result<()>;
    pub fn dup(&self, parent, src_client, src_object) -> Result<Handle>;          // NV04_DUP_OBJECT
    pub fn map_cpu(&self, device, memory, offset, length, flags) -> Result<*mut u8>; // NV04_MAP_MEMORY (kernel VA)
    pub fn unmap_cpu(&self, device, memory, ptr) -> Result<()>;
    pub fn map_dma(&self, device, virt, memory, offset, length, MapFlags) -> Result<u64>;  // NV04_MAP_MEMORY_DMA -> GPU VA
    pub fn map_dma_at(&self, ..., va_offset: u64) -> Result<u64>;                 // at an offset inside the VA range
    pub fn unmap_dma(&self, device, virt, memory, gpu_va) -> Result<()>;
    pub fn alloc_memory(&self, device, subdevice, MemLocation, size, MemAttrs) -> Result<Handle>;
    pub fn alloc_va(&self, device, vaspace, size, align, PageSize) -> Result<(Handle, u64)>;
    pub fn bar1_map(&self, subdevice, memory, offset, length) -> Result<*mut u8>;
    pub fn destroy(self) -> Result<()>;                   // free with a status
}
pub enum MemLocation { Vidmem, Sysmem }
pub enum PageSize { Default, Small /*4K*/, Big /*64K*/ }
pub struct MemAttrs { contiguous: bool, page_size: PageSize, extra_flags: u32 }   // default: 4K, non-contiguous
pub struct MapFlags(pub u32);                             // default: CACHE_SNOOP_ENABLE | PAGE_SIZE_4KB
```

### How each call maps onto RM

The mappings follow UVM's `nv_gpu_ops.c`, the reference kernel client.

- **`alloc_memory`**: vidmem is `NV01_MEMORY_LOCAL_USER` under the
  **subdevice**; sysmem is `NV01_MEMORY_SYSTEM` under the **device**. Both use
  `NV_MEMORY_ALLOCATION_PARAMS { owner = HEAP_OWNER_RM_KERNEL_CLIENT,
  type = IMAGE, attr = LOCATION | COHERENCY | PHYSICALITY | PAGE_SIZE }`.
  - Sysmem is CPU-cached and GPU-snooped (`COHERENCY_CACHED`).
  - Its pages come from our `nv_alloc_pages`.
- **`alloc_va`**: an `NV50_MEMORY_VIRTUAL` under the device, with
  `flags = VIRTUAL | ALIGNMENT_FORCE | ALLOCATE_KERNEL_PRIVILEGED` and
  `hVASpace`. RM returns the VA in `offset`.
- **`map_dma`**: `NV04_MAP_MEMORY_DMA(hDma = that virtual object)`. The input
  dmaOffset is relative to the range; the output is the absolute GPU VA.
- **`map_cpu`** (for a kernel client the result is a kernel virtual address):
  - Sysmem: `nv_alloc_kernel_mapping`.
  - Vidmem: a BAR1 window through `os_map_kernel_space`.
  - A `VOLTA_USERMODE_A` object: its BAR0 doorbell page.
- **Handles**: RM generates them; pass 0, which `alloc` does. A caller-chosen
  handle also works (`alloc_with_handle`), which is what NVK expects.

### Notes for the UAPI lane

- NVK's ioctls (alloc, control, free, map) map 1:1 onto `alloc_raw`,
  `control_raw` and the map calls. Keep one `Client` per DRM file.
- **Embedded pointers.** Control blocks that contain user pointers (the
  non-`_V2` controls, e.g. `NV2080_CTRL_CMD_GPU_GET_ENGINES`) must be copied
  in and fixed up by the UAPI layer; with KERNEL parameter location RM
  dereferences them directly. Prefer the `_V2` (inline) controls.
- **User mmap.** RM's half is now implemented:
  - `nv_alloc_user_mapping` returns the page's physical address.
  - `os_match_mmap_offset` finds the page back.
  - `nv_add_mapping_context_to_file` (C glue) records the mapping per fd.
    Read it back with `lnv_mapping_context_take(fd, &mut lnv_mmap_ctx_t)` from
    `lnv_rmapi.h`.
  - Wiring `NV_ESC_RM_MAP_MEMORY` and mmap through `rm_ioctl` is the UAPI
    lane's job. `nv_acquire_fabric_mgmt_cap` answers NOT_SUPPORTED: there is
    no fabric manager on a single GeForce.

### The ABI check

`params::ABI` lists 161 (name, value) pairs: struct sizes, field offsets,
classes, controls, flag bit values and USERD offsets. The C table in
`lnv_rmapi.c` (164 entries) has the same names, computed with NVIDIA's
headers and `DRF_DEF`/`NV_OFFSETOF`. `client::abi_check` compares them. The
self-test's first step refuses to continue on any mismatch. A transcription
error therefore costs one log line in QEMU, not a misread parameter block on
the GPU. **When you add a struct or constant to `params.rs`, add it to both
tables.**

## 2. The PCIe upstream-bridge warnings (fixed)

**Root cause.** RM reaches the upstream bridge (00:01.0) through
`clPcieRead*`. Those calls need `PDB_PROP_CL_PCIE_CONFIG_ACCESSIBLE`, which
`objClInitPcieChipset` sets once `clStorePcieConfigSpaceBaseFromMcfg` finds the
ACPI MCFG table. On UEFI, `GetRsdtXsdtTablesAddr` asks
`os_get_acpi_rsdp_from_uefi` first and gives up when it fails; it does not
fall back to the BIOS-area scan. Ours returned `NV_ERR_OPERATING_SYSTEM`, as
nvidia-haiku's does, so MCFG was never read. That produced the "PCIE config
space is inaccessible!" lines and the `clPcieReadWord() failed` assertion at
`chipset_pcie.c:3013`.

**Fix.**
- `os_get_acpi_rsdp_from_uefi` returns the Limine RSDP. It is passed in
  `BootEnv.rsdp_phys` from the same one-line hook in `kernel/src/init.rs`.
  This mirrors Linux, which returns `efi.acpi20`.
- RM then walks RSDP → XSDT → MCFG with cached `os_map_kernel_space` mappings,
  as on Linux (`PDB_PROP_OS_CACHED_MEMORY_MAPPINGS_FOR_ACPI_TABLE`), and maps
  the bridge's ECAM page itself (uncached) per access.
- `nvrm.nomcfg=1` restores the old behaviour, as a bisection switch.
- Related: `os_pci_init_handle` now caches one handle per bus/device/function.
  RM calls it for every chipset config access, and each call used to leak a
  `Box` and print a log line.

**QEMU evidence.** The `abi` step repeats the walk with the same callbacks and
logs:

```
[nvrm] +30ms os_get_acpi_rsdp_from_uefi: RSDP at 0x7fb7e014
[nvrm] acpi: RSDP 0x7fb7e014: signature ok, checksum ok, revision 2, XSDT 0x7fb7d0e8, RSDT 0x7fb7d074
[nvrm] acpi: MCFG 0x7fb76000 (checksum ok): ECAM base 0xe0000000 segment 0 buses 0x0-0xff
```

**On the PC, expect:**
- `os_get_acpi_rsdp_from_uefi: RSDP at 0x…` in step 10, where
  `cb+ os_get_acpi_rsdp_from_uefi` used to be.
- More `os_map_kernel_space … C` lines: ACPI tables, cached.
- `… UC` 4 KiB maps around 0xf8000000-ish: ECAM pages.
- No "inaccessible" line and no `clPcieReadWord` assertion.
- RM may print `*** Chipset has no definition! (vendor ID 0x8086, device ID
  0x591f)` (a WARNING). This is harmless: Kaby Lake's host bridge is not in
  RM's chipset table, and MCFG covers it.

## 3. The self-test (`nvrm.selftest=ce`)

It runs in the `[nvrm]` thread as bring-up step 11, `selftest`, after
`step adapter ok`. `nvrm.stop_after=adapter` skips it. Each sub-step resets the
`[nvrm-wd]` watchdog budget (`nvrm.timeout`, 90 s); watchdog lines name it as
`step selftest/<substep>`.

- **Log flushing:** the stick log is flushed before `channel` and before every
  GPU step.
- **Waits:** every semaphore wait is bounded at 2 s and is a watchdog abort
  point (`time::relax`).
- **Parking:** if the watchdog parks the thread, the console is handed back
  first.

### Options

| option | meaning |
|---|---|
| `nvrm.selftest=ce` | the hardware self-test (needs `nvrm.start=1`, which the stick sets) |
| `nvrm.selftest=abi` | device independent: ABI check, ACPI/MCFG walk, client and NV0000 controls; runs without a GPU |
| `nvrm.selftest_stop=<step>` | stop after a step, then tear down |
| `nvrm.selftest_skip=<step>[,<step>]` | skip steps; later GPU steps need `channel` and `submit` |
| `nvrm.selftest_hold=<s>` | how long the colour bars stay up (10) |
| `nvrm.selftest_loops=<n>` | round trips in `loop` (1000) |
| `nvrm.selftest_fbphys=<hex>` | GOP surface VRAM offset, used only if RM gives no console memory object, and only after the marker check |
| `nvrm.nomcfg=1` | withhold the RSDP (section 2) |

Steps: `abi client device info vaspace memory channel submit memset intr fb
loop teardown`.

### What each log line means (in order)

Every line is `[nvrm] +<ms>ms selftest <step>: …`. Each step ends with
`selftest <step> ok|FAILED (<ms> ms)` or `… skipped (…)`.

1. **abi**
   - `161 constants/sizes/offsets match NVIDIA's headers, 0 mismatches`.
   - Then the `acpi:` RSDP and MCFG lines, followed by
     `ACPI MCFG as RM walks it: 1 ECAM segment(s)`.
2. **client**
   - `NV01_ROOT client handle 0xc1d0000N`.
   - `RM build: driver '570.86.16' version 'rel/gpu_drv/r570/r571_57-276'`.
   - `attached GPU ids: [<id>]`.
   - `gpu <id>: device instance 0, subdevice 0, flags …`.
3. **device**
   - `device 0x…, subdevice 0x…`.
   - `<n> classes: …`: hex list, which should contain `c56f c7b5 c361 a06c 90f1`.
   - `using host class 0xc56f, CE class 0xc7b5`.
   - Any `class … NOT in the class list` line is the first thing to report.
4. **info**
   - `name: NVIDIA GeForce RTX 3070`.
   - `FB: ram KiB=8388608 … bus width=256 ram type=… bar1 KiB=262144 …` and
     `VRAM 8192 MiB (ram type …), BAR1 256 MiB`.
   - `GR: …` and `<g> GPCs (mask …), <t> TPCs, <s> SMs`. The 3070 has 6 GPCs,
     23 TPCs and 46 SMs.
   - `<n> engines: …`, then `CE<i> (engine 0x…): caps … [GRCE]`.
   - `copy engine for the self-test channel: CE<i> (…, first async (non-GRCE) CE)`.
5. **vaspace**: `FERMI_VASPACE_A 0x…: base … size …`.
6. **memory**
   - One line per buffer: `pb` (128 KiB), `gpfifo`, `userd`, `sem` (sysmem),
     `src` and `dst` (1 MiB sysmem), `vid` (1 MiB vidmem). Each shows its RM
     handle, GPU VA and CPU pointer.
   - Then `CPU access to the sysmem mappings ok`.
7. **channel**
   - `channel group …` → `channel …: GPFIFO VA …, USERD = sysmem …, chid <n>`.
   - `copy engine object …`.
   - `doorbell: USERMODE … mapped at … work-submit token 0x…`.
   - `channel scheduled`.
   - `USERD after scheduling: GP_GET 0 GP_PUT 0`.
8. **submit** (the first real GPU work)
   - `push words: …`: the exact method words.
   - `GPFIFO[0] = …`.
   - `semaphore = 0x1 after <n> us: the channel runs, the CE executes methods
     and writes sysmem`.
9. **memset**
   - `CE filled 1 MiB of vidmem … copied it to sysmem` and
     `vidmem fill (via CE read-back): 262144 of 262144 words correct`.
   - `2 x 1 MiB copies in … us (… MB/s)` and
     `round trip: 262144 of 262144 words correct`.
10. **intr**: `semaphore released; <n> ISR(s), <m> bottom half(s)`. A
    non-stall interrupt was requested; 0 ISRs is reported but not a failure.
11. **fb**
    - `GOP framebuffer: phys 0xe0000000, 1024x768, pitch 4096, 3072 KiB`.
    - `RM console memory 0x…: VRAM offset 0x… (vidmem), contiguous segment …`:
      **the GOP surface's VRAM offset** (see section 4).
    - `console memory mapped at GPU VA …`.
    - `CPU wrote a marker … the CE read 1024 of 1024 marker pixels back`.
    - `8 colour bars (112x384 px each, at x=64 y=192) drawn by the CE`.
    - `bar centres correct: 8/8 via CE read-back, 8/8 via CPU reads of BAR1`.
    - `holding the bars on screen for 10 s` and `<n> repaints during the hold`.
12. **loop**
    - `1000 fill(64 KiB)+semaphore round trips in <t> ms: <r> per s; latency
      min/avg/max`.
    - `last fill: 16384 of 16384 words correct`.
    - `during the loop: <n> ISRs, 0 RM Xid lines, 0 stub hits`.
13. **teardown**: `<n> RM calls ok, 0 failed`.

The run ends with `==== selftest PASSED: 13 passed, 0 failed, 0 skipped; RM
Xid lines 0, stubs hit 0 ====` and `==== bring-up finished: rm_init_adapter
OK, selftest PASSED ====`.

### What the user sees on screen

- The console scrolls the step lines, then text output pauses.
- **8 vertical colour bars** fill the middle half of the screen: white,
  yellow, cyan, green, magenta, red, blue, grey, each 112 px wide.
- A thin coloured strip at the top-left (the marker) may flash first.
- The bars stay for about 10 s. Then the console text is repainted and
  `selftest PASSED` scrolls by.
- The serial log and the stick log carry everything, including the lines
  printed while the console was paused.

### Decision tree

1. **`abi` FAILED (mismatch).** The `params.rs` transcription is wrong for
   this RM build: fix the named entry. This cannot happen with the committed
   tree; QEMU checks it.
2. **`client` fails.**
   - `NV01_ROOT alloc FAILED`: RM's API is not initialised. `rm_init_rm` must
     have returned true.
   - `attached GPU ids: []` with a GPU present: `rm_init_adapter` did not
     attach it. Look for the adapter step's result.
3. **`device` fails** (`NV01_DEVICE_0 FAILED: 0x…`). Report the status name.
   `NV_ERR_INVALID_STATE` usually means the GPU is not attached or is lost.
4. **`info` failures are warnings.** Only the CE choice depends on them. If
   `GET_ENGINES_V2` failed, the guess `COPY2` is used; report the line.
5. **`vaspace` or `memory` fails.** Report the status.
   - `NV_ERR_NO_MEMORY` on sysmem: check the `nv_alloc_pages: OUT OF MEMORY`
     lines.
   - Failure on `vid`: RM heap problem; the `info` step shows heap free.
6. **`channel` fails.** The failing object and status are printed.
   - `KEPLER_CHANNEL_GROUP_A`: the engine type. Try another CE with
     `nvrm.selftest_skip=…`, or report the `CE … caps` lines.
   - `AMPERE_CHANNEL_GPFIFO_A`: look at the `NVRM:` lines just above. USERD in
     sysmem or a GPFIFO VA problem would show there.
   - `GET_WORK_SUBMIT_TOKEN` or `GPFIFO_SCHEDULE`: report both statuses.
7. **`submit` TIMEOUT.** The test prints a `channel state:` line.
   - `GP_GET` still 0: the host never fetched the entry. Check the doorbell
     (USERMODE mapping, token), the USERD placement and the scheduling.
   - `GP_GET == GP_PUT` but the semaphore is unchanged: the host read the
     push buffer but the CE did not release. Look for `NVRM: … Xid` lines
     (RC, MMU fault). An MMU fault names a VA: compare it with the VAs in the
     `memory` lines.
   - Either way the run goes on to teardown.
8. **`memset` mismatch.**
   - All words wrong (still 0): the copy never reached `dst`. A coherency
     issue would show here.
   - Pattern correct but the round trip wrong: the vidmem mapping or a copy
     length. The first 4 bad words are printed with their offsets.
9. **`fb`.**
   - `NV01_MEMORY_FRAMEBUFFER_CONSOLE FAILED`: RM did not reserve the console.
     The test then tries physical offset `nvrm.selftest_fbphys` (default 0),
     guarded by the marker check.
   - `the CE target is NOT the scanout surface`: the marker check failed, so
     nothing was drawn. Rerun with `--cmdline
     "nvrm.selftest_fbphys=<VRAM offset from the 'RM console memory' line>"`.
   - Bars drawn per the log but not visible: the display engine scans out
     something else after GSP init. Report the `bar centres correct` line
     (CPU-via-BAR1 count).
10. **`loop`: Xid lines or a slow max latency.** Report the line. An
    occasional slow iteration (ms) is scheduling noise; Xid lines are real
    errors.
11. **`teardown` failures.** Report them. Frees during an RC can fail; the
    client free at the end releases everything else.
12. **The machine hangs in a GPU step.** The log was flushed at the step start.
    Bisect with `nvrm.selftest_stop=channel`, then `=submit`.
13. **Regressions in the bring-up itself.**
    - The only change before `rm_init_adapter` returns is the RSDP: RM now
      walks MCFG and reads the bridge's config space.
    - If step 10 now fails or hangs where boot 1 passed, rerun with
      `--cmdline "nvrm.nomcfg=1"` to confirm, and report the `NVRM:` lines.

## 4. The GOP surface's VRAM offset: how it is found

**Before GSP.** BAR1 is in physical mode and identity-maps VRAM. The GOP
framebuffer the UEFI firmware set up is at BAR1+0 (lane nvprobe), so it sits
at VRAM offset 0 of BAR1's physical window.

**When RM switches BAR1 to virtual,** it keeps the console visible.
`kbusInitBar1` maps RM's **reserved console region** (`fbRegion[0]`, sized
from `ReservedConsoleDispMemSize`) at BAR1 VA 0
(`kbusIsPreserveBar1ConsoleEnabled`). That is why the console survived GSP
init in boot 1.

**The self-test asks RM for that region directly:**
- It allocates `NV01_MEMORY_FRAMEBUFFER_CONSOLE` (class 0x76, parent device,
  kernel-privileged). This is the object nvidia-modeset uses
  (`nvRmMapFbConsoleMemory`); it wraps the reserved console memdesc.
- `NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR` on it returns the **physical VRAM
  offset** (`memOffset`) and aperture. That value is logged.
- The object is mapped into the self-test's VA space and the CE writes there
  (virtual mode), so the offset is not even needed to draw.

**It is verified rather than assumed.** The CPU writes a marker into the
first 4 rows through BAR1 (the address the console draws to), reads back
once to flush posted writes, and the CE copies the same rows from the
target. Only if all 1024 marker pixels match does it draw.

**Fallbacks:**
- If the VA mapping fails, physical CE writes at `memOffset`.
- If the object cannot be allocated, physical CE writes at
  `nvrm.selftest_fbphys` (default 0), behind the same marker check.

**Not used:** `NV0076_CTRL_CMD_NOTIFY_CONSOLE_DISABLED`. It would drop RM's
BAR1 console mapping.

## 5. Hardware instructions

```sh
# Mac, in the repo (after merge); --quick needs a previous build-all for x86_64
diskutil list external
./scripts/nvidia-test-stick.sh /dev/diskN --quick --cmdline "nvrm.selftest=ce"
#   (full build instead of --quick if build-all has not run recently)
```

`--cmdline` may be given more than once. The kernel and the script both take
the last value of a repeated key, so for example `--cmdline "nvrm.start=0"`
really turns the bring-up off. The script prints the final command line,
which here is `nvprobe.dumpvbios=1 nvrm.start=1 nvrm.selftest=ce`.

On the PC:
1. Boot the stick (UEFI boot menu).
2. Wait for `==== rm_init_adapter OK`.
3. Watch for the colour bars. They appear a few seconds later and stay about
   10 s.
4. Wait for `==== selftest PASSED|FAILED` and `bring-up finished`, then log in
   as root/root and run `sync; poweroff`.

Back on the Mac:

```sh
diskutil unmountDisk /dev/diskN
sudo ./scripts/read-usb-log.sh /dev/rdiskN > nvrm-boot2.log
grep -a 'selftest\|Xid\|NVRM\|rsdp\|acpi' nvrm-boot2.log
```

Bisection examples:
- `--cmdline "nvrm.selftest=ce nvrm.selftest_stop=info"`: objects and queries
  only, no GPU work.
- `--cmdline "nvrm.selftest=ce nvrm.selftest_skip=fb"`: no screen writes.

## 6. QEMU evidence (Mac)

| test | result |
|---|---|
| Feature-off kernels vs main `7bf2d40`, built in this worktree | **byte-identical**: x86_64 `2de940a9…90fa`, aarch64 `00a9ef25…67c0` |
| x86_64 nvidia kernel, `nvrm.selftest=abi`, no GPU | `rm_init_rm -> true`; `abi: 161 … match, 0 mismatches` (C table 164: 3 entries not used from Rust); RSDP/XSDT/MCFG walk ok (ECAM 0xe0000000, buses 0-ff); client `0xc1d00001`; `RM build: driver '570.86.16'`; attached ids `[]`; `NV01_DEVICE_0 -> 0x40 … [NV_ERR_INVALID_STATE] (expected to fail)`; teardown ok; `selftest PASSED: 3 passed`; stubs hit 0 |
| x86_64 nvidia kernel, `nvrm.start=1 nvrm.selftest=ce`, no GPU | `no NVIDIA display controller present (nvrm.start=1: nothing to start)` and `nvrm.selftest=ce: no NVIDIA GPU, self-test skipped`; login OK |
| x86_64 nvidia kernel, `-device edu`, `nvrm.fakegpu=1234:11e8 nvrm.selftest=ce nvrm.start=0 nvrm.start=1` | the last `nvrm.start` wins (bring-up runs); steps 0-9 ok, firmware crc `0x28191e52`, MSI 0x41; adapter rehearsal "FAILED" by design, so the self-test is not reached |
| `runtests regress`, default kernels | aarch64 **15/15** (HVF); x86_64 **15/15** (TCG) |
| `runtests regress`, x86_64 nvidia kernel with `nvrm.start=1 nvrm.selftest=ce` (no GPU: clean skip) | **15/15** |
| `nvidia-test-stick.sh` cmdline composition (function run alone) | `"nvprobe.dumpvbios=1 nvrm.start=1" + "nvrm.selftest=ce nvrm.timeout=180 nvrm.start=0 klog.testpanic"` → `nvprobe.dumpvbios=1 nvrm.start=0 nvrm.selftest=ce nvrm.timeout=180 klog.testpanic` |

**Not testable in QEMU:** everything from `device` on with a real GPU. That
covers GSP RPCs for every allocation, the channel, doorbell, CE methods,
console memory and RM's MCFG walk inside `rm_init_adapter`. The push words
come from NVIDIA's DRF macros (C encoders in `lnv_rmapi.c`), and the method
sequences follow UVM's Volta/Ampere CE HAL and RM's `channel_utils.c`.

## 7. Files

- **`ports/openrm/glue/lnv_rmapi.c` / `.h`** (new):
  - the RM API wrappers;
  - the ABI table;
  - push encoders (`lnv_push_set_object`, `_ce_memset` (1-D and pitch 2-D),
    `_ce_copy`, `_ce_sem_release`, `lnv_gp_entry`);
  - `nv_add_mapping_context_to_file` and `lnv_mapping_context_take`.
- **`ports/openrm/build.sh`**: compiles every glue source (`lnv_glue`,
  `lnv_rmapi`).
- **`drivers/nvidia/src/client.rs`** (new): the API.
- **`drivers/nvidia/src/params.rs`** (new): constants, parameter blocks and
  the ABI list.
- **`drivers/nvidia/src/selftest.rs`** (new).
- **`drivers/nvidia/src/device.rs`**:
  - step 11 `selftest`;
  - `substep`, so the watchdog names sub-steps;
  - the `abi` thread with no GPU;
  - console release when the thread is parked.
- **`drivers/nvidia/src/lib.rs`**:
  - new options;
  - the last value of a repeated key wins;
  - the cmdline buffer is now 1 KiB;
  - `BootEnv.rsdp_phys`;
  - the Xid line counter.
- **`drivers/nvidia/src/platform.rs`**: `os_get_acpi_rsdp_from_uefi`,
  `probe_mcfg_like_rm`, `nv_acquire_fabric_mgmt_cap`.
- **`drivers/nvidia/src/mem.rs`**: `nv_alloc_user_mapping`,
  `nv_free_user_mapping`, `os_match_mmap_offset`; the map log budget is now 160.
- **`drivers/nvidia/src/pcios.rs`**: the handle cache.
- **`drivers/nvidia/src/stubs.rs`**: regenerated, 159 stubs.
- **`kernel/src/init.rs`**: the same single cfg'd line, now also passing
  `rsdp_phys`.
- **`scripts/nvidia-test-stick.sh`**: repeatable `--cmdline` with key
  de-duplication, and self-test instructions.

**Shared cache.** The glue changed, so the shared cache's `libnvrm.a` must be
rebuilt. `ports/openrm/build.sh` does it (incrementally), and
`nvidia-test-stick.sh` runs it. A kernel linked against an old archive fails
with undefined `lnv_rm_*` symbols: rerun `build.sh`. This lane built in a
scratch `OPENRM_BUILD_DIR`.

## 8. Known limits / follow-ups

- **No error notifier on the channel** (`hObjectError = 0`; RM supports
  `ERROR_NOTIFIER_TYPE_NONE`). Channel errors show only as RM `Xid` lines,
  which are counted. Add a notifier for the UAPI lane, since NVK expects one.
- **USERD, GPFIFO and push buffer are in sysmem.** No BAR1 writes are on the
  submission path, so UVM's dummy BAR1 read is not needed. NVK puts USERD in
  vidmem through BAR1: that path is untested here.
- **The non-stall interrupt goes through RM's ISR and bottom half.** No
  events are registered (`nv_get_event` is still a stub), so the interrupt is
  only counted.
- **`rm_ioctl` itself (the `/dev/nvidia*` escape path) is not wired.** Kernel
  clients do not need it, and the UAPI layer can call `Client` directly.
