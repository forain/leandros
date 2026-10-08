# lane/nvuapi — 2026-10-08 — Mesa NVK on the RTX 3070 through a nouveau-UAPI layer over OpenRM

Branch `lane/nvuapi`, based on main `3268125`. Not pushed. Milestones M5
(NVK opens the GPU) and M6 (offscreen rendering and compute) are built and
QEMU-validated; **not yet run on the PC**. The first hardware boot is the
test. Section 7 has the user instructions, section 8 the decision tree.

## 1. What this lane adds

```
 nvktest / any Vulkan app
   Mesa 25.3.6 NVK (libvulkan_nouveau.so, unmodified)
     libdrm: drmGetDevices2 (sysfs) + nouveau ioctls + syncobjs + mmap
 ───────────────────────────────── /dev/dri/renderD128 (and card0) ──────
   servers/drm  ── VERSION: "nouveau" 1.4.0, errno pass-through
   drivers/src/drm_device_interface.rs
     ├─ gpu_uapi seam: driver-private ioctls, GEM_CLOSE, mmap tokens,
     │  PRIME, per-open teardown, sysfs PCI identity  ─────────┐
     └─ syncobjs: binary + TIMELINE (syncobj_core), fence      │
        oracle = virtio watermark | backend fence_done         │
   drivers/nvidia/src/uapi.rs  (registered after rm_init_adapter) ◄┘
     one RM client per open: device, subdevice, FERMI_VASPACE_A,
     NV01_MEMORY_SYSTEM / LOCAL_USER, NV50_MEMORY_VIRTUAL binds,
     AMPERE_CHANNEL_GPFIFO_A + engine objects, doorbell, fences
   OpenRM 570.86.16 + GSP-RM (lanes openrm, nvshim, nvclient)
```

- **`drivers/src/gpu_uapi.rs`** (new): the seam. A `UapiBackend` is a table
  of function pointers registered once by a driver in another crate.
  Unregistered (every QEMU build), each hook is one atomic load.
- **`drivers/src/syncobj_core.rs`** (new): sync-object payloads as
  Linux's `dma_fence_chain` model — binary or timeline, with collected
  history (`base`), point lookup that stands for "this point and every
  earlier one", WAIT_FOR_SUBMIT / WAIT_AVAILABLE semantics. Pure logic.
- **`drivers/src/drm_device_interface.rs`**:
  - syncobjs rebuilt on `syncobj_core`. Fence retirement is generalised:
    `fence_is_done(id)` asks the backend for ids with bit 63 set and keeps
    the in-order watermark for everything else (virtio/v3d unchanged).
  - TIMELINE_WAIT, QUERY, TIMELINE_SIGNAL, TRANSFER implemented; answered
    only while a backend is registered (else ENOSYS, as before), and
    `DRM_CAP_SYNCOBJ_TIMELINE` reports 1 only then.
  - WAIT and TIMELINE_WAIT accept both struct sizes (with and without
    Linux 6.4's `deadline_nsec`). **Bug fix:** the libdrm in the image sends
    the 40-byte WAIT (0xC02864C3), which used to fall through to "unimplemented
    ioctl"; no current virtio client issues it, Mesa's vk runtime does.
  - Hooks for the backend: VERSION, the driver-private block, GEM_CLOSE,
    mmap tokens, PRIME export/import/release, per-open release.
  - `wait_fences`, `syncobj_in_fences`, `syncobj_install`,
    `gpu_fence_irq_hint` for the backend's EXEC/VM_BIND.
- **`drivers/src/lib.rs`**: `DriverError::Errno(i32)` — an exact errno
  (NVK maps ENODEV to VK_ERROR_DEVICE_LOST).
- **`servers/drm`**: Errno pass-through; render-node VERSION from the
  backend; registers the VFS sysfs hook.
- **`servers/vfs`**: `/sys/dev/char/226:{0,128}/device/{vendor,device,
  subsystem_vendor,subsystem_device,revision,class,uevent,config}` answered
  by the backend while it is registered (narrow overlay: exact files only,
  never directories — see the 2026-09-15 /sys shadowing regression), and
  `/proc/cmdline`.
- **`kernel/src/init.rs`**: one line, the cmdline for `/proc/cmdline`.
- **`userland/init`**: `nvrm.vktest=1` on the kernel command line runs
  `/bin/nvk-autotest` once in the background (spared by the stray sweep).
- **`drivers/nvidia/src/uapi.rs`, `uapi_abi.rs`, `uapi_vm.rs`** (new): the
  translation layer, the UAPI structs (sizes pinned by const asserts taken
  from a compiled probe of Mesa's headers), and the pure VM_BIND bookkeeping.
- **`drivers/nvidia`** params/glue: the new RM classes, controls, flags and
  structs — all in the ABI table, checked against NVIDIA's headers at run
  time — and two push encoders (`lnv_push_host_fence`, `lnv_gp_entry_sync`).
- **`ports/nvk-tests/`** (new): `nvktest` (C, loader-less, dlopens the ICD),
  shaders + generated SPIR-V, Docker build, `nvk-autotest.sh`.
- **`scripts/mkfs-f2fs-populated.py`** stages `/bin/nvktest` and
  `/bin/nvk-autotest`; **`scripts/nvidia-test-stick.sh`** builds nvktest and
  adds `nvrm.vktest=1` by default.
- **`drivers/uapi-tests/`** (new): host unit tests (15) for `syncobj_core`
  and `uapi_vm`.

## 2. Design decisions

### One RM client per DRM open
`Open` = NV01_ROOT + NV01_DEVICE_0 + NV20_SUBDEVICE_0, created on the open's
first nouveau ioctl. Closing the last fd frees the client: RM drops every
channel, mapping, VA space and buffer with it, so a crashing app leaks
nothing. A separate global **keeper** client owns the doorbell
(VOLTA_USERMODE_A, one BAR0 page for all channels; the work-submit token
names the channel), the device-wide queries (VRAM used, PTIMER), and the
duplicates (`NV04_DUP_OBJECT`) that keep an exported buffer alive after its
creator is gone.

### VA space: userspace-managed, binds are fixed-address RM objects
NVK owns its VA heap ([4 KiB, 2^38), top-down) and tells the kernel at
VM_INIT which window is the kernel's ([2^39, 2^40)). The layer creates a
default FERMI_VASPACE_A and treats the whole space as externally managed:

- every MAP outside a sparse region is its **own NV50_MEMORY_VIRTUAL range
  allocated at the fixed address** (`FIXED_ADDRESS_ALLOCATE`), plus one
  `NV04_MAP_MEMORY_DMA` with `PAGE_KIND_VIRTUAL`. The PTE kind therefore
  comes from the range, which is what lets one buffer be bound with
  different kinds (NVK binds images with their own kind): kinds 1-5
  (Z16, S8, S8Z24, ZF32_X24S8, Z24S8) are requested through the depth/stencil
  allocation attributes nvidia-haiku's NVK backend uses; everything else
  (0, 6 = GENERIC_MEMORY) is a plain IMAGE range, which RM gives
  GENERIC_MEMORY. A mismatch between the requested and the chosen kind is
  logged (`WARNING pte kind ... RM chose ...`).
- a sparse region is one SPARSE virtual range; binds inside it are
  `DMA_OFFSET_FIXED` mappings into that range.
- UNMAP and overlapping MAPs are "unmap the old mapping, map the surviving
  pieces" (RM cannot shrink a mapping); `uapi_vm.rs` decides this, with
  drm_gpuvm's semantics, and is host-tested.
- The kernel's own buffers (GPFIFO, kernel push ring, fence semaphore) are
  mapped at fixed addresses in the kernel window.
- Known overlap: GSP-RM keeps [4 GiB, 4.5 GiB) of every VA space for its
  own mappings. NVK allocates from 2^38 downwards, so a collision needs
  ~250 GiB of VA in use; a bind there would fail with a logged RM error.

Binds are executed synchronously: wait for the in-fences, bind, signal the
out-syncobjs with an already-signalled fence.

### Memory placement
- **CPU-mappable** (`MAPPABLE`, or GART-only): `NV01_MEMORY_SYSTEM`,
  **physically contiguous**, cached, GPU-snooped. The mmap token
  (`map_handle`) is its physical address; `mmap` maps it write-back.
- **Not mappable** (VRAM): `NV01_MEMORY_LOCAL_USER`, 4 KiB pages.
- BAR1 (256 MiB, no Resizable BAR) is **not** used for user mappings:
  NVK's host-visible "VRAM" type (it advertises one, sized like BAR1) is
  backed by system memory. Correct and coherent, slower than VRAM for the
  GPU. A BAR1 path would need on-demand window management and WC mappings
  (`gpu_uapi::set_wc_range` is in place for it).
- GEM_INFO reports the real placement (GART or VRAM).

### Channels
CHANNEL_ALLOC creates, per channel:
- sysmem GPFIFO (4096 entries), USERD, a 64 KiB kernel push ring, the fence
  semaphore page, and an error-notifier page wrapped in an
  `NV01_CONTEXT_DMA` (`hObjectError`);
- `AMPERE_CHANNEL_GPFIFO_A` with `engineType = GRAPHICS` under the device,
  as nvidia-haiku's NVK backend does (falls back to an explicit
  `KEPLER_CHANNEL_GROUP_A` if RM refuses), `NVA06F BIND`, `GPFIFO_SCHEDULE`,
  `GET_WORK_SUBMIT_TOKEN`.
NVIF SCLASS offers c7b5 (copy), c797 (3D), c7c0 (compute), 902d (2D),
a140 (inline-to-memory); NVIF NEW allocates them on the channel. RM (with
GSP) allocates and promotes the GR context buffers itself when the first GR
object is created; no PROMOTE_CTX call is needed (X547 confirms on GA10x).
NVK binds the 3D and compute classes with its own SET_OBJECT; the copy
class on subchannel 4 works without one on a graphics channel, as on nouveau.
A refusal of the 2D or inline-to-memory class is logged and answered as
success (NVK allocates both and drives neither on Ampere).

### EXEC and fences
One GPFIFO entry per push (`NO_PREFETCH` -> `SYNC_WAIT`), then a kernel
segment — Linux nouveau's gv100 fence sequence with a 64-bit payload:
`SEM_ADDR/PAYLOAD`, `SEM_EXECUTE(RELEASE, WFI, 64-bit)`, `MEM_OP` SYS
membar, `NON_STALL_INTERRUPT` — then GP_PUT and the doorbell. The fence id
is `bit63 | ctx << 40 | seq` (ctx never reused). `fence_done` reads the
semaphore; lock-free, so the syncobj table can ask it under its spinlock.
In-fences: fences of the same channel are skipped (in-order); others are
waited for on the CPU (10 s bound, then ETIME). Waiters are woken by the
non-stall interrupt (the RM ISR calls `uapi::irq_hint`), by a 1 ms re-probe
deadline, and by the 100 Hz DRM tick while anyone waits.

### Robustness
- `nvrm.uapi_log` 0/1/2. Level 1 (default) logs every ioctl (sampled after
  4000 lines). Errors are always logged as `[nvuapi] ... ERROR`.
- Watchdog thread `[nvuapi-wd]`: a channel whose semaphore makes no progress
  for `nvrm.uapi_timeout` (10 s) while work is outstanding, or whose RM error
  notifier fires, is marked dead (its fences report done, waiters wake),
  descheduled (`GPFIFO_SCHEDULE` off), and every later EXEC on it answers
  ENODEV (NVK: VK_ERROR_DEVICE_LOST). New `Xid` lines from RM are reported.
- Nothing waits unboundedly: GPFIFO space 5 s, kernel segment reuse 5 s,
  in-fences 10 s, CPU_PREP 30 s.

## 3. ioctl coverage

| ioctl | status | notes |
|---|---|---|
| VERSION | done | `nouveau` 1.4.0 on card0 and renderD128 |
| GET_CAP | done | SYNCOBJ 1, SYNCOBJ_TIMELINE 1 (backend registered), PRIME import+export |
| GETPARAM | done | PCI_VENDOR/DEVICE, BUS_TYPE (PCIE), FB_SIZE, AGP_SIZE (free RAM), CHIPSET_ID (MC arch info), GRAPH_UNITS (GPCs, TPCs), PTIMER_TIME (NV2080 TIMER_GET_TIME), HAS_BO_USAGE, HAS_PAGEFLIP, EXEC_PUSH_MAX (1023), VRAM_BAR_SIZE, VRAM_USED (total - heap free), HAS_VMA_TILEMODE = 0 |
| SETPARAM | EINVAL | deprecated |
| CHANNEL_ALLOC / FREE | done | graphics runlist only (nouveau's engine-select hack answers ENOSYS) |
| NVIF NEW (device, engine objects) | done | |
| NVIF MTHD NV_DEVICE_V0_INFO | done | PCIE, chipset, family AMPERE, VRAM, chip and name strings |
| NVIF SCLASS | done | the five engine classes |
| NVIF DEL | done | |
| VM_INIT | done | refuses a second VM_INIT (EEXIST) and a window under 64 MiB |
| VM_BIND MAP / UNMAP / SPARSE / RUN_ASYNC | done | synchronous; MAP without a handle = UNMAP (NVK's sparse unbind) |
| EXEC | done | binary + timeline in/out syncs |
| GEM_NEW / GEM_INFO | done | |
| GEM_CPU_PREP | done | waits for the open's channels; NOWAIT -> EBUSY |
| GEM_CPU_FINI | done | no-op |
| GEM_PUSHBUF | partial | only the `nr_push == 0` "channel dead?" probe (ENODEV when dead) |
| GEM_CLOSE | done | |
| mmap of a GEM | done | sysmem buffers |
| PRIME HANDLE_TO_FD / FD_TO_HANDLE | done | sysmem buffers with a power-of-two page count export their frames; others are token-only (import works, mmap of the dmabuf does not) |
| SYNCOBJ CREATE / DESTROY / WAIT / RESET / SIGNAL | done | WAIT in both sizes |
| SYNCOBJ TIMELINE_WAIT / QUERY / TIMELINE_SIGNAL / TRANSFER | done | backend registered only |
| SYNCOBJ HANDLE_TO_FD / FD_TO_HANDLE / EVENTFD | ENOSYS | external semaphores; not needed for M5/M6 |
| SVM_INIT / SVM_BIND, GROBJ, NOTIFIEROBJ | ENOSYS | not used by NVK |

## 4. Log lines (in boot order)

| line | meaning |
|---|---|
| `[nvuapi] +..ms ==== nouveau UAPI ready: ... answer 'nouveau' 1.4.0 for NVIDIA GeForce RTX 3070 (10de:2484, GA104 chipset 0x174), VRAM 8192 MiB, BAR1 256 MiB, 6 GPCs / 23 TPCs; engines [c7b5, c797, c7c0, 902d, a140] ...` | backend registered after `rm_init_adapter` (and the self-test, if requested) |
| `[nvuapi] ... NOT available: <why>` | the device facts or the doorbell could not be set up; nothing registered |
| `nvrm.vktest=1: starting /bin/nvk-autotest in the background` | init |
| `==== nvk-autotest: start ====`, `... renderD128 present after N s` | the script |
| `[ENV ] ...`, `nvktest preflight ...` | nvktest's environment and DRM caps (should say `nouveau`, SYNCOBJ_TIMELINE 1) |
| `[nvuapi] +..ms open N: RM client ..., device ..., subdevice ...` | NVK opened the render node |
| `open N: VM_INIT: FERMI_VASPACE_A ...` | VA space created |
| `open N: NVIF NEW device`, `NVIF MTHD DEVICE_INFO: chipset 0x174 family 0xd ...` | NVK identifying the device |
| `open N: CHANNEL_ALLOC -> channel C (RM ..., class 0xc56f, graphics runlist, ...) in X ms` | a channel |
| `open N: NVIF SCLASS channel C: [c7b5, c797, c7c0, 902d, a140]` | |
| `open N: NVIF NEW class 0xc797 on channel C -> RM ... (X ms)` | GR context created by RM (the first one takes longest) |
| `open N: GEM_NEW ... -> handle ...` | buffers |
| `open N: VM_BIND k op(s) ... -> ok` | binds (level 2 lists each op) |
| `open N: EXEC channel C pushes P ... -> seq S GP_PUT G` | a submission |
| `nvktest info: PASS (1 device(s); using "NVIDIA GeForce RTX 3070" ...)` | **M5** |
| `nvktest fill / timeline / compute: PASS` | copy, syncobj timeline, compute shader |
| `nvktest triangle: PASS (red=18432 px, FNV-1a 0x...)` + `nvktest img|` ASCII art + `ppm32` base64 | **M6** |
| `==== nvktest done: P passed, F failed, ...` | |
| `[nvuapi] ... ERROR ...` | anything that failed, with RM's status name |
| `channel C of open N: HUNG ...` / `RM error notifier fired: info32 ...` | watchdog |

## 5. Expected success lines

```
[nvuapi] +...ms ==== nouveau UAPI ready: /dev/dri/card0 + renderD128 answer 'nouveau' 1.4.0 for NVIDIA GeForce RTX 3070 (10de:2484, GA104 chipset 0x174) ...
nvktest info: PASS (1 device(s); using "NVIDIA GeForce RTX 3070" 10de:2484)
nvktest device: PASS (...)
nvktest fill: PASS (65536 words verified per variant)
nvktest timeline: PASS (GPU signal 1, host signal 2, timeout on 3 all correct)
nvktest compute: PASS (65536-element C=A+B verified in host memory and in VRAM)
nvktest triangle: PASS (red=18432 px, FNV-1a 0x........)
nvktest triangle_depth: PASS (red=18432 px, ...)
nvktest timestamps: PASS (... ns for a 16 MiB fill)
==== nvktest done: 8 passed, 0 failed, 0 skipped, 0 timeout ====
```
The lavapipe reference checksum is 0x02C0FDC5; NVK should match if its
rasterisation is exact (a different value with the right pixel count is
not a failure — compare the ASCII art).

## 7. User instructions

```sh
# Mac, in the repo (after merge). Full build (needed once: init and the
# shared DRM layer changed, so --quick's old initrd/userland are stale):
diskutil list external
./scripts/nvidia-test-stick.sh /dev/diskN
#   (later runs: --quick)
```
The default command line is now `nvrm.start=1 nvrm.vktest=1`. Useful
additions: `--cmdline "nvrm.uapi_log=2"` (every ioctl with arguments),
`--cmdline "nvrm.uapi_timeout=30"`, `--cmdline "nvrm.vktest=0"` (no NVK
test), `--cmdline "nvrm.uapi=0"` (no nouveau UAPI at all),
`--cmdline "nvrm.selftest=ce"` (the CE self-test first; it is independent).

On the PC:
1. Boot the stick (UEFI boot menu).
2. Wait for `==== rm_init_adapter OK`, then `==== nouveau UAPI ready`.
3. The login prompt appears; leave the keyboard alone. About 20 s after
   the render node exists, `==== nvk-autotest: start ====` and the nvktest
   lines scroll by. Wait for `==== nvk-autotest: done ====` (1-3 minutes).
4. Log in as root/root, `sync; poweroff`. If the screen stops changing for
   5 minutes, hold the power button (the log is flushed every 2 s).

Back on the Mac:
```sh
diskutil unmountDisk /dev/diskN
sudo ./scripts/read-usb-log.sh /dev/rdiskN > nvrm-boot3.log
grep -a 'nvuapi\|nvktest\|nvk-autotest\|Xid\|NVRM' nvrm-boot3.log
# the 32x32 triangle, rebuilt from the log:
grep -a 'nvktest ppm32|' nvrm-boot3.log | sed 's/.*nvktest ppm32| //' | tr -d '\r' | base64 -d > tri32.ppm
```

## 8. Decision tree for the first hardware result

1. **No `nouveau UAPI ready` line.**
   - `NOT available: <why>` — the reason names the RM call (class list,
     arch info, doorbell). Report it.
   - Neither line — `rm_init_adapter` failed or the bring-up stopped
     earlier; the nvshim/nvclient trees apply.
2. **`nvktest info: 0 physical devices`** with the UAPI ready.
   - `preflight` shows the render node's driver name. Not `nouveau` →
     the VERSION hook did not apply (report the line).
   - No `[nvuapi] open N` line at all → NVK never opened the node: libdrm
     did not classify it. Check `cat /sys/dev/char/226:128/device/vendor`
     (expect `0x10de`) and `.../uevent` from the shell.
   - `open N` lines, then an `ioctl nr 0x.. -> -E` line → that ioctl is
     the refusal; the errno and the preceding ERROR line say why.
3. **`info` PASS, `device` FAIL** (vkCreateDevice). Look for the first
   `ERROR` after `CHANNEL_ALLOC`: a refused engine class (`engine class
   0xc797: NV_ERR_...`), a GEM/VM_BIND failure, or an EXEC failure. A
   `VA range ... NV_ERR_...` means a fixed VA allocation was refused
   (alignment/collision) — report the address.
4. **`fill` TIMEOUT / HUNG.** The GPU did not finish a submission.
   - `RM error notifier fired: info32 31 (MMU fault)` → a VA NVK used is
     not mapped (compare with the VM_BIND lines at `nvrm.uapi_log=2`).
   - `info32 13/69 (GR exception)` → a method/class problem.
   - `HUNG` without a notifier → the channel never ran: check that
     `GP_PUT` advanced in the EXEC lines; rerun with `nvrm.selftest=ce` to
     confirm the CE channel path still works on this boot.
5. **`fill` PASS, `compute` FAIL** — shaders run on the SMs: if the values
   are wrong but no hang, report the first mismatches nvktest prints. A hang
   with a GR exception → GR context problem (report the notifier line).
6. **`compute` PASS, `triangle` FAIL** — 3D pipeline. Wrong pixels with the
   right count → tiling/kind; zero red pixels → render target binding.
7. **`triangle` PASS, `triangle_depth` FAIL** — depth PTE kinds: look for
   `WARNING pte kind ... RM chose ...`.
8. **`timeline` FAIL** — syncobj timeline path; the failing step is printed.
9. **Machine hang.** The log was flushed every 2 s: the last `[nvuapi]`
   line names the last ioctl. Rerun with `nvrm.uapi_log=2`.

## 9. QEMU evidence (Mac: aarch64 HVF, x86_64 TCG)

| test | result |
|---|---|
| `drivers/uapi-tests` (host, `cargo test --target aarch64-apple-darwin`) | **15/15**: binary lifecycle, timeline points/query/GC, prefix lookup (TRANSFER), CPU signal + out-of-order add, fence helpers; VM_BIND map/unmap, splits, overlap replace, validation, sparse regions, forget/clear, failed re-map, 2000-op random consistency |
| `runtests --suite regress`, default kernels | aarch64 **15/15**, x86_64 **15/15** |
| `runtests --suite drm --virgl`, aarch64 (drmsmoke: syncobjs, PRIME, flips) | **PASS** |
| COSMIC desktop, aarch64 virgl (`ffsession --nofirefox`) | greeter, panel, cosmic-term up; screenshot shows panel, wallpaper, terminal |
| x86_64 nvidia kernel, `nvrm.start=1 nvrm.vktest=1 nvrm.selftest=abi`, no GPU | ABI **210** constants/sizes/offsets match NVIDIA's headers, 0 mismatches; selftest=abi PASSED; init starts nvk-autotest; nvktest preflight: render node `virtio_gpu`, SYNCOBJ_TIMELINE=0 (nothing registered); `nvktest info: 0 physical devices (no nouveau DRM device)`, exit 3, no crash |
| `runtests --suite regress`, x86_64 nvidia kernel, `nvrm.start=1` | **15/15** |
| nvktest against lavapipe (Docker, both arches; the tool's own check) | 8/8 PASS, triangle FNV-1a 0x02C0FDC5 |

Feature-off kernels are **not** byte-identical to main: the DRM layer
(syncobj rewrite, backend hooks), the VFS (/proc/cmdline, sysfs hook) and
init.rs changed for every build. Behaviour on virtio is unchanged except the
WAIT size fix (section 1); the regress, drm and desktop runs above are the
evidence.

**Not testable in QEMU:** everything past registration — the RM calls of
the UAPI (fixed VA ranges, channels, engine objects, EXEC on hardware),
NVK's device probe against a real `nouveau` node, and rendering.

## 10. Review fixes and known limits

A read-only review before the hardware run found, and this lane fixed:
fixed-address NV50_MEMORY_VIRTUAL allocations without
`NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE` (RM would raise the alignment to the
64/256 KiB compression page and refuse NVK's 4 KiB-aligned binds — the run
would have failed at vkCreateDevice); the channel-group fallback not
scheduling the group; zero-length pushes refused; a watchdog subtraction
that could wrap; the in-fence bound now follows `nvrm.uapi_timeout`.

Known limits (not needed for M5/M6):
- unbinding inside a sparse region leaves invalid (faulting) PTEs, not
  sparse zero-reading ones, and binds inside a sparse region take the
  region's kind (generic) — sparse residency is advertised by NVK but not
  exercised by nvktest;
- PRIME of VRAM or non-power-of-two sysmem buffers is token-only (no
  frames), and syncobj fd/sync-file export is ENOSYS: presenting through a
  compositor needs both;
- card0 also answers `nouveau` (single-device design); on the PC the
  graphical login refuses to start anyway (no GL renderer);
- host-visible memory is physically contiguous sysmem (large HOST_VISIBLE
  allocations can fail with ENOMEM under fragmentation); mmap tokens are
  checked by base only, and a user mapping can outlive its buffer;
- every ioctl at log level 1 goes to the UART and the console: slow, but
  that is the point of the first run (`nvrm.uapi_log=0` for speed).
