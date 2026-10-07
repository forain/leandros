# lane/nvshim — 2026-10-07 — G2: OpenRM OS shim, ready for the first RTX 3070 run

Branch `lane/nvshim`, base main `ba76d39`. Not pushed. Mac: x86_64 under TCG,
aarch64 under HVF.

**Outcome.** `drivers/nvidia` now implements the callbacks nvidia-haiku really
implements, on the lane-kfacil kernel facilities, and runs the Linux
`nv_pci_probe` + `nv_start_device` sequence step by step in a kernel thread,
with a watchdog, a persisted log, and opt-in gating. It has been rehearsed in
QEMU as far as QEMU allows: the callbacks against the `edu` device (41/41),
the whole start sequence on a stand-in PCI function, the watchdog in both
hang modes, the firmware loaded from a USB-stick root, and the stick log read
back on the host. The first real GPU access (`rm_is_supported_device`,
`rm_init_adapter`) can only happen on the PC.

## 1. Callback coverage

`llvm-nm -u nv-kernel.o`: 344 OS symbols.

| provided by | count |
|---|---|
| C glue (`ports/openrm/glue/lnv_glue.c`) | 22 (+3: `nv_get_ctl_state`, `nv_get_usermap_access_params`, `nv_get_disp_smmu_stream_ids`) |
| hand-written Rust | **158** (was 43) |
| generated stubs (log + fail) | 164 (was 282) |

Of the 146 symbols nvidia-haiku really implements, 141 are implemented here.
The other 5 belong to the user-mapping / fabric path of RM ioctls, which a
later lane will add: `nv_add_mapping_context_to_file`, `nv_alloc_user_mapping`,
`nv_free_user_mapping`, `os_match_mmap_offset` and `nv_acquire_fabric_mgmt_cap`.
Everything nvidia-haiku panic-stubs stays a stub, which logs and fails: libspdm,
NVSwitch, power management, NUMA, dma-buf, peer IO, user page locking, temporary
files, vGPU. A few panic-stubbed callbacks are implemented anyway because it
costs nothing: `os_get_random_bytes`, `os_queue_work_item`/`os_flush_work_queue`,
the nano timers, port I/O, the `os_string_*` helpers and the NVLink libc
wrappers. `ports/openrm/gen-shim.py --report` prints the per-symbol table.

### Implemented callbacks, by module

- **`sync.rs`** (sched::ksync, sched::irqctx):
  - Spinlocks (`RawSpinLock`, IRQ-saving).
  - Sleeping mutex, rwlock and semaphore. Acquisition fails with
    `NV_ERR_INVALID_REQUEST` in hard-IRQ context or under a spinlock, as Linux
    does. Contended waits run in 5 s slices; each expired slice logs one line
    and is a watchdog abort point.
  - Wait queues, implemented as `KCompletion` (Linux `complete_all`).
  - `os_is_isr` = `irqctx::in_irq`.
  - `os_semaphore_may_sleep` = `can_sleep`.
- **`time.rs`**:
  - Clocks: TSC monotonic and realtime.
  - `os_delay_us` spins below 1 ms and sleeps from 1 ms up. `os_delay` sleeps.
  - `os_schedule`.
  - `os_get_cpu_frequency` (TSC kHz).
  - Nano timers: worker-mode `HrTimer` calling `rm_run_nano_timer_callback`.
  - RC timer: a no-op returning 0, as in nvidia-haiku.
  - **`relax()`**: called from `os_get_current_tick[_hr]` and the delay
    spin. RM's GSP RPC poll (`_kgspRpcRecvPoll`) and register polls spin on the
    clock with an empty `osSpinLoop`. Kernel threads run with IRQs masked, so
    at most once per ms per CPU the clock read opens an IRQ window /
    reschedule point (`kthread::cond_resched`), when the context may sleep.
    That keeps the CPU's tick alive (no `[WDOG]`) and gives the watchdog
    somewhere to park a runaway thread.
- **`mem.rs`**:
  - `nv_alloc_pages` / `nv_free_pages` on the buddy allocator:
    - Contiguous requests take one naturally aligned block and return its tail
      in aligned pieces.
    - Scattered requests use `page_size` chunks.
    - DMA limit: 4 GiB until `nv_set_dma_address_size`; below the top of RAM
      that means `alloc_below`.
    - All pages are zeroed. UC and WC pages are clflushed.
  - `nv_alloc_kernel_mapping`: the HHDM alias for WB contiguous ranges, else a
    kva `vmap` with the allocation's cache type.
  - `os_map_kernel_space`: always a kva `ioremap` with the requested type
    (UC / WC / WB). Every call is logged.
  - `os_flush_cpu_write_combine_buffer` (sfence).
  - User copies.
  - Constant answers, as nvidia-haiku.
- **`pcios.rs`**:
  - Handle = boxed `PciFunction`; full 4 KiB config space, with the ECAM page
    mapped at handle creation so ISR reads work. Every config write is logged.
  - x86 port I/O.
- **`work.rs`**: `os_queue_work_item` runs `rm_execute_work_item` on the private
  `nvrm-q` WorkQueue; `os_flush_work_queue`.
- **`fw.rs`**: `nv_get_firmware` / `nv_put_firmware` read
  `/lib/firmware/nvidia/570.86.16/gsp_{ga10x,tu10x}.bin` with `request_firmware`
  (a page list, vmapped WB). It also preloads the image with timing, an ELF
  magic check and a CRC-32 of the whole file. `gsp_log_*` is absent, so RM
  says "no GSP-RM logs will be printed (non-fatal)".
- **`platform.rs`**:
  - `nv_get_screen_info` reports the Limine GOP framebuffer when it lies in
    one of this GPU's BARs. With the framebuffer at BAR1's base, RM keeps a
    BAR1 mapping for the console once BAR1 goes virtual.
  - Process identity, caps (opaque tokens), file-private/events (no device
    files yet), ACPI (all `NOT_SUPPORTED`), I2C (NULL), NVLink libc wrappers.
  - `os_bug_check` parks the RM thread instead of panicking.
- **C glue**:
  - nvidia-haiku's registry tweak
    `RMProcessNonStallIntrInLocklessIsr = 0`: nonstall interrupts go to the
    bottom half.
  - `cpu_numa_node_id = -1`.
  - `nv_state_t` accessors (flags, interrupt line, queue, BARs) and the ctl
    state.

Each callback starts with `cb!("name")`: its first call logs
`[nvrm] +<ms> cb+ <name> [<context> cpuN]`, and every call is counted and
recorded in a 32-entry ring.

## 2. The bring-up sequence (device.rs)

The hook in `kernel/src/init.rs` is one cfg'd line. `init()` records the
options and the framebuffer. A second one-line hook, after userspace init has
been spawned, starts the `[nvrm]` kernel thread. Starting it there keeps init's
pid unchanged, and the system work queue and the request_firmware backend
already exist by then.

| # | step | what | touches the GPU |
|---|---|---|---|
| 0 | rminit | `rm_init_rm` (+ `nvrm.reg` overrides) | no |
| 1 | handle | `os_pci_init_handle`, config dump (ids, COMMAND/STATUS, caps, ext caps, MSI/MSI-X control) | config reads |
| 2 | bars | `size_bars` (decode off for µs, restored), `nv_state_t`, BAR0/1/3 → REGS/FB/IMEM, GOP framebuffer vs BAR1 | config writes (restored) |
| 3 | locks | `rm_init_event_locks` | no |
| 4 | supported | `rm_is_supported_device` (maps BAR0, reads PMC_BOOT_0/42) | MMIO reads |
| 5 | private | `rm_init_private_state`, `rm_set_rm_firmware_requested`, `nv->queue` | — |
| 6 | root | wait for the real root (`root_ready`, `nvrm.roottimeout`) | no |
| 7 | firmware | preload + CRC of the GSP image | no |
| 8 | pcicmd | COMMAND \|= MEM \| IO \| MASTER | yes |
| 9 | msi | `EnableMSI` registry, `request_msi` → ISR (`rm_isr`, BH on `nvrm-bh` → `rm_isr_bh`), `NV_FLAG_USES_MSI` | yes |
| 10 | adapter | `rm_init_adapter`: FWSEC, firmware, radix3/WPR, booter, GSP-RM INIT_DONE (polled) | yes |

- Without `nvrm.start=1`, only step 0 runs, then the thread stops: detection
  only.
- The log is written to the stick (`klog::flush_now`) before steps 2, 4, 8 and
  10 and at the end.
- The `[nvrm-wd]` watchdog:
  - Logs a heartbeat every 10 s of a step and dumps the last 32 callbacks every
    30 s.
  - After `nvrm.timeout` (default 90 s; the root step gets `roottimeout`+30)
    it dumps the state and sets ABORT.
  - The RM thread parks at its next sleepable callback: a clock read,
    delay, schedule or lock wait.
  - If it has not parked 10 s later, the watchdog clears bus mastering,
    masks the MSI and dumps again.

### Command line

| option | meaning |
|---|---|
| `nvrm.start=1` | full bring-up; default is `rm_init_rm` + detection only |
| `nvrm.stop_after=<step>` | stop after a step; names in the table above |
| `nvrm.timeout=<s>` | watchdog budget per step (90) |
| `nvrm.roottimeout=<s>` | wait for the root filesystem (180) |
| `nvrm.loglevel=<n>` | RM message floor, 0 INFO … 4 ERRORS (0 with start=1, else 1) |
| `nvrm.reg=Name:val,…` | RM registry dwords before init (e.g. `RmMsg:…`, `EnableMSI:0`) |
| `nvrm.nomsi=1` | skip MSI (bisection) |
| `nvrm.testdev=VVVV:DDDD` | callback self-test (QEMU `-device edu` = 1234:11e8) |
| `nvrm.fakegpu=VVVV:DDDD` | rehearse the start sequence on a non-NVIDIA function (skips the 3 NVIDIA-only RM calls) |
| `nvrm.fakehang=1\|2` | with fakegpu: hang in step 10 (1 = RM-style clock poll, 2 = no OS calls) |

## 3. Scripts

- `scripts/mkfs-f2fs-populated.py`: for x86_64, stages
  `gsp_ga10x.bin` (63,330,032 B) and `gsp_tu10x.bin` from
  `$OPENRM_CACHE/firmware/570.86.16/` into `/lib/firmware/nvidia/570.86.16/`.
  It skips them silently when the cache is absent. The tag is read from
  `ports/openrm/version.sh`.
- `scripts/build-all.sh --nvidia`: fetches the source and firmware if missing,
  runs `ports/openrm/build.sh` (incremental), and builds the x86_64 standard
  kernel with `--features nvidia`. The direct-boot kernel and aarch64 are
  unchanged.
- `scripts/nvidia-test-stick.sh <device> | --image out.img [--quick] [--cmdline "…"] [--gpt] [--yes]`:
  - Fetches the source and firmware.
  - Builds everything: `build-all --arch x86_64 --nvidia`. With `--quick` it
    only builds libnvrm, the kernel and the root image.
  - Checks that the kernel has OpenRM and prints the firmware CRC to expect.
  - Runs `write-usb.sh x86_64` with
    `LEANDROS_USB_EXTRA_CMDLINE="nvprobe.dumpvbios=1 nvrm.start=1 <extra>"`.
- `ports/openrm/build.sh` and `gen-shim.py` accept `OPENRM_BUILD_DIR`. This lane
  built its glue in a scratch directory so the shared cache's archive was not
  swapped under other checkouts. After this merges, the next
  `ports/openrm/build.sh` (or `build-all --nvidia`) rebuilds the glue in the
  shared cache. **A kernel built against an old `libnvrm.a` fails to link:**
  `nv_get_ctl_state` & co. would be defined twice. Fix: rerun `build.sh`.

## 4. QEMU evidence (Mac)

| test | result |
|---|---|
| Default kernels (no feature) vs main `ba76d39`, both built in this worktree | **byte-identical**: x86_64 `b7974d85…f554`, aarch64 `e6fb45c2…037a` |
| x86_64 `--features nvidia`, `nvrm.start=1`, no NVIDIA device | `[nvrm] OpenRM 570.86.16 linked; no NVIDIA display controller present (nvrm.start=1: nothing to start)`; login OK |
| `nvrm.testdev=1234:11e8` + `-device edu` | **41 pass, 0 fail** (list below) |
| `nvrm.fakegpu=1234:11e8` | steps 0–9 ok. Firmware 63,330,032 B, ELF, crc32 `0x28191e52` (= host `zlib.crc32`). MSI vector 0x41. Step 10 "FAILED" by design |
| `fakehang=1 timeout=15` | heartbeat at 10 s, state dump + ABORT at 15 s, `RM thread (pid 4) PARKED in os_get_current_tick` 1 ms later; system fine |
| `fakehang=2 timeout=15` | ABORT at 15 s; after 10 s more, `RM thread did not reach an abort point`, `bus mastering off (0x0407 -> 0x0403)`, dump. **The spinning CPU is lost** (`[WDOG]` every 10 s) and the guest became sluggish; the log is complete |
| USB stick image (`write-usb.sh --image`, cmdline `… nvrm.start=1 nvrm.fakegpu=…`) booted with `LEANDROS_USB_STICK` | firmware loaded **from the stick's root** (`ls /lib/firmware/nvidia/570.86.16` in the guest shows both files); `scripts/read-usb-log.sh stick.img` returns every step line through `bring-up finished` |
| `runtests regress`, x86_64 nvidia kernel + `nvrm.start=1`, firmware-staged root | **15/15 PASS** |
| `runtests regress`, default kernels | aarch64 **15/15 PASS**. x86_64: 14/15 on the first run (`timertest realtime_is_not_uptime`); rerun PASS. This is the byte-identical main kernel, so it is pre-existing flakiness |

Testdev checks:
- PCI config space:
  - handle, dword/word/byte reads;
  - byte write + restore at 0x3C;
  - extended offset 0x100 through ECAM;
  - offset 4096 rejected.
- Probe path up to `rm_is_supported_device`: `size_bars`, `nv_state_t` BAR,
  `rm_init_event_locks` / `rm_destroy_event_locks`.
- BAR0 MMIO: `os_map_kernel_space` UC, edu id, liveness write/read.
- MSI: delivered, and the ISR releases an RM semaphore.
- DMA through `nv_alloc_pages` / `nv_alloc_kernel_mapping` under a 28-bit limit:
  - RAM→device from a WB contiguous page (HHDM mapping);
  - device→RAM into page 1 of a scattered UC allocation, read through a UC
    vmap with offset 16: 0 bytes differ.
- Allocator: buddy tail decomposition self-check, 37-page contiguous block
  alignment, 4 MiB in 2 MiB chunks, all pages freed.
- Locks, wait queue, delays:
  - mutex try/hold/release; rwlock reader/writer exclusion;
  - spinlock forbids sleeping, released allows it;
  - `os_wait_uninterruptible` woken from another thread after 31 ms;
  - `os_delay(20)` 21 ms; `os_delay_us(300)`;
  - random bytes.
- Firmware: preload, `nv_get_firmware(GSP, GA10X)` gives 63,330,032 B at an
  ELF; `GSP_LOG` gives NULL.

Not testable in QEMU: anything RM does to a real GPU (steps 4 and 10), RM's ISR,
and the console behaviour after GSP init.

## 5. What the user does (hardware test)

```sh
# on the Mac, in the repo (after this lane is merged)
diskutil list external                          # find the stick: /dev/diskN
./scripts/nvidia-test-stick.sh /dev/diskN       # full build + write (asks before writing)
#   faster when build-all ran recently:  ./scripts/nvidia-test-stick.sh /dev/diskN --quick
```

On the PC (RTX 3070, UEFI, Resizable BAR off):
1. Boot the stick from the firmware boot menu.
2. Watch the `[nvrm]` lines.
3. Wait for `==== bring-up finished: … ====`, or 3 minutes.
4. If a shell is usable, log in as `root`/`root` and run `sync; poweroff`.
   Otherwise hold the power button. The log is flushed before every risky step
   and every 2 s.

Back on the Mac:
```sh
diskutil unmountDisk /dev/diskN
sudo ./scripts/read-usb-log.sh /dev/rdiskN > nvrm-boot1.log      # latest boot
sudo ./scripts/read-usb-log.sh /dev/rdiskN --all > nvrm-all.log  # last 4 boots
grep -a 'nvrm\|NVRM\|NVPROBE\|WDOG\|KSYNC\|PANIC' nvrm-boot1.log
```
Send `nvrm-boot1.log` back. Bisection runs reuse the same command with, for
example, `--cmdline "nvrm.stop_after=private"`.

## 6. Reading the log on the real PC

In order of appearance:
- `[NVPROBE] …`: the read-only probe from lane nvprobe, plus the VBIOS
  base64 dump (`nvprobe.dumpvbios=1`).
- `[nvrm] found 01:00.0 10de:2484 class 0300`: the GPU was found. The audio
  function 10de:228b is class 04 and must not appear.
- `[nvrm] OpenRM 570.86.16: 1 GPU(s); nvrm.start=1 …`: the options in effect.
- `[nvrm] +0ms [nvrm] thread pid N`: the bring-up thread is running. It
  starts after userspace init.
- `cb+ <name> [ctx cpuN]`: first use of an OS facility. The last `cb+` before a
  hang says what RM was doing.
- `step N/10 <name>: …` then `step <name> ok|FAILED (ms)`: the step progress.
- Step 1: config dump. Expect COMMAND 0x0007 or 0x0003, MSI at cap 0x05 and
  MSI-X 0x11.
- Step 2: BAR lines. Expect BAR0 mem32 16 MiB, BAR1 mem64 pref 256 MiB,
  BAR3 mem64 32 MiB, BAR5 io. Then
  `GOP framebuffer 0xe0000000 … = BAR1+0x0: at BAR1 base, RM keeps a BAR1 console mapping`.
  Any other offset prints a warning.
- `os_map_kernel_space phys=… size=0x1000000 UC -> 0xfffff0…`: RM mapped BAR0.
  More of these lines follow later (BAR1/BAR3 pieces, WC).
- Step 4: `rm_is_supported_device -> 0x0`.
- Step 5: RM private state.
- Steps 6–7: `firmware … 63330032 bytes …, magic 7f 45 4c 46 (ELF)` and
  `crc32=0x28191e52`.
- Step 8: `COMMAND 0x… -> 0x…7`.
- Step 9: `registry EnableMSI = Some(1)` and `MSI enabled: vector 0x4N -> BSP`.
- Step 10: `rm_init_adapter …`. `NVRM: …` lines are RM's own messages, mixed in
  with `nv_alloc_pages …` (logged when large), `nv_get_firmware: type 0 chip
  family 4 -> gsp_ga10x.bin`, `os_map_kernel_space`, the first `ISR #n`
  lines, and `bottom half #n`.
- `==== rm_init_adapter OK in N ms: GSP-RM is up (INIT_DONE) ====` is the
  success criterion. It is followed by `device name: NVIDIA GeForce RTX 3070`
  and the BAR1 line.
- `STUB (unimplemented OS callback, returns failure): <name>`: RM reached a
  callback nvidia-haiku never needed. **Report the name.**
- `[nvrm] wd: …`: a step is taking long.
- `WATCHDOG: aborting …` and `PARKED in …`: the watchdog gave up. The state
  dump just above it shows the step, the last 32 callbacks with ages, callback
  counts, ISR/BH counts, sysmem/kva usage and PCI COMMAND/STATUS.
- `NVRM BUG CHECK`: RM's own fatal error. The thread parks.
- `==== bring-up finished: <how> ====` plus the callback count table: the
  end of the run.

## 7. Decision tree for the first result

1. **No `[nvrm]` lines at all.** The kernel is not the nvidia one.
   `nvidia-test-stick.sh` checks for "OpenRM 570.86.16" in the kernel. Or the
   kernel died earlier; look for `PANIC`.
2. **`found` but `thread pid` never shows.** The kthread did not start; init
   probably never got there. Look at the `[INIT]` lines.
3. **Stuck or failing in `handle` / `bars`.** No ECAM (`extended-config=NO`)
   is harmless for these steps. `UNASSIGNED` BARs mean the firmware did not
   assign them (Above-4G decoding?). Report the BAR lines.
4. **`supported` fails** (`rm_is_supported_device -> 0x…`):
   - Check that `os_map_kernel_space` for BAR0 succeeded.
   - If PMC_BOOT_0 read as 0xffffffff in NVPROBE, the GPU is powered down or
     decode is off.
   - Otherwise RM does not know the device id/revision: report the
     `NVRM:` lines.
5. **Root/firmware step fails.** `file not found`: the image was built
   before `fetch.sh`. Rerun the stick script. A CRC mismatch means a corrupt
   copy.
6. **`msi` fails.** `request_msi failed`. Try `--cmdline "nvrm.nomsi=1"`:
   GSP INIT_DONE is polled, so step 10 can still be reached without MSI.
7. **Step 10 (`adapter`), where the real work happens.** Look at the last
   `NVRM:` lines and the last `cb+`:
   - **Fails before any `nv_get_firmware` line** (VBIOS/FWSEC stage): RM
     could not read or parse the VBIOS, or FWSEC-FRTS failed. Compare RM's
     VBIOS messages with the `[NVPROBE]` VBIOS dump (CRC, images). Typical
     lines are `kgspExtractVbiosFromRom`, `FWSEC`, `FRTS`. Next step: check
     the ROM-shadow / PROM read path. RM reads the VBIOS through BAR0+0x300000
     itself; if nvprobe could and RM could not, the difference is in the PCI
     ROM enable bit or timing.
   - **`STUB … <name>` right before the failure**: implement that callback.
     Most likely candidates: `nv_dma_map_*` (would mean RM wants an IOMMU
     path; check `nv_requires_dma_remap`) and `os_get_page`.
   - **`nv_alloc_pages: OUT OF MEMORY`**: the DMA limit or contiguity. The
     line shows the limit. Look for a missing
     `nv_set_dma_address_size: 47 bits` line before it.
   - **Fails after the firmware was handed out, around "booter" / "WPR" /
     "falcon"** (RM messages like `kgspExecuteBooterLoad`,
     `Booter … failed`, falcon mailbox values): the GSP boot sequence on the
     falcons failed. Usual causes:
     - Sysmem buffers not visible to the device: a cache-type problem.
       Next run: `--cmdline "nvrm.reg=…"` is not enough here. The fix is to
       make all `nv_alloc_pages` WB and compare; it is a 1-line change in
       `mem.rs::cache_mode`.
     - Radix3 pages above the DMA width.
     - The WPR2 region being already up from a previous driver; a power cycle
       fixes that.
     Report the mailbox values.
   - **`INIT_DONE` timeout** (RM: `Timeout waiting for … GSP_INIT_DONE`,
     Xid 119, RPC history dump; or the watchdog firing in step 10 with the
     last callbacks being `os_get_current_tick`): GSP-RM started but never
     answered.
     - If `ISR #` lines are absent, interrupts never arrived. The init
       handshake is polled, so that alone is not fatal, but it points at the
       message queues: the shared sysmem buffers again (coherency/cache type).
     - Check `nv_get_screen_info` and the console: RM reserves console VRAM
       (`ReservedConsoleDispMemSize`). If the framebuffer offset was not 0,
       note it.
     - Next run: `nvrm.timeout=300` to rule out slowness. Then the
       all-WB experiment above.
   - **Machine hard-hangs in step 10** (log ends mid-step, no watchdog
     lines): a PCIe fatal error or a CPU-side MMIO hang. The log was flushed
     at the step start. Next run with `--cmdline "nvrm.loglevel=0"` (already
     the default) and compare the last `os_map_kernel_space` / `cb+` lines.
     If it is the console (fb writes after BAR1 went virtual), try
     `nvrm.reg=…` to disable console preservation, or boot with the
     console off.
   - **`rm_init_adapter OK`**: G2 is done. Next lane: the `rm_ioctl` path
     (DRM server, nouveau UAPI for NVK), user mappings, events.
8. **The screen stops updating during step 10 but the log continues.** RM
   moved BAR1 to virtual mode and the console mapping was not preserved
   (framebuffer offset ≠ 0), or display init replaced the scanout. This is
   harmless for G2; the stick log is the source of truth.

## 8. Known limits / follow-ups

- **A loop that never calls the OS cannot be parked.** Its CPU stays lost with
  IRQs masked, and the watchdog only quiesces the GPU (fakehang=2 shows the
  degraded system). RM's waits all go through `gpuCheckTimeout` and the OS
  clock, so that should not happen.
- **MSI targets the BSP only** (kfacil). The ISR runs `rm_isr` in hard-IRQ
  context and the bottom half on the `nvrm-bh` WorkQueue (task context).
  `rm_gpu_handle_mmu_faults` before `rm_isr` (Linux) is not called, as in
  nvidia-haiku.
- **UC/WC system memory keeps a WB HHDM alias**, as in nvidia-haiku. RM only
  touches it through the typed mapping, and the lines are flushed at
  allocation.
- **`os_pat_supported` is false**, as in nvidia-haiku, so RM avoids its
  PAT-dependent paths. PAT WC works in mm::kva if RM wants it later.
- **No `/dev/nvidia*`** yet: `nv_get_file_private` returns NULL and events are
  dropped.
- **The stick test needs the image built after `fetch.sh`.** The script
  ensures that.
- **QEMU validation note.** This worktree's images came from the main checkout,
  which is on `lane/usbhid`. A root image built with main's `ba76d39` mkfs
  script against that userland fails `exectest` (script interpreter cases).
  The root images for the tests above were therefore built with the main
  checkout's own mkfs script plus this lane's firmware hunk.
