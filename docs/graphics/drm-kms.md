# DRM and KMS

LeandrOS implements the Linux Direct Rendering Manager interface — mode setting, buffer management, PRIME, fences and sync objects — closely enough that unmodified libdrm, Mesa, smithay (cosmic-comp) and `kmscube` drive the display through it. This page describes the device model, the ioctl surface and the design rules that keep a multithreaded compositor and several GPU clients from stepping on each other.

## Overview

| Piece | Where | Role |
|---|---|---|
| DRM server | [`servers/drm/src/lib.rs`](../../servers/drm/src/lib.rs) | Registers `/dev/dri/card0` and the render node `/dev/dri/renderD128` in the VFS, routes ioctls/read/poll/mmap to the interface |
| UAPI handler | [`drivers/src/drm_device_interface.rs`](../../drivers/src/drm_device_interface.rs) | Every Linux DRM and virtgpu ioctl, BO lifetime, fences, syncobj, master, flip events |
| Object model | [`drivers/src/drm/`](../../drivers/src/drm/) | CRTC/connector/encoder/plane objects, property trees, modes, auth, the software present path |
| KMS / EDID | [`drivers/src/kms.rs`](../../drivers/src/kms.rs) | Reads EDID from the virtio-gpu device and configures the boot mode |
| Device | [`drivers/src/virtio_gpu.rs`](../../drivers/src/virtio_gpu.rs) | See [virtio-gpu.md](virtio-gpu.md) |
| Intel Gen9 device | [`drivers/src/i915/`](../../drivers/src/i915/mod.rs) | The real laptop's GPU: KMS, outputs, hot-plug, power wells; see [the i915 backend](#the-i915-backend) |

Like every server, the DRM server is a `no_std` crate linked into the kernel whose port handler is called directly on the sending task's thread (see [Servers](../servers/README.md)). Its ioctl handler therefore runs **synchronously in the caller's address space**, so ioctl arguments are dereferenced directly — under the rule that no user memory is touched while any spinlock is held (see [Invariants](../kernel/invariants.md)): input structs are copied in before the device lock is taken and outputs copied back after it is dropped. That rule is not decorative; a demand fault taken under a scheduler-adjacent spinlock froze every vCPU once (`82d0cc3`), and `GETCONNECTOR` was still writing user memory under the device lock as late as October 2026.

Userspace opens a node, authenticates, and drives the pipeline with standard ioctls. libdrm discovers the device through a synthetic, read-only sysfs (`/sys/class/drm`, `/sys/dev/char/226:0`, `uevent` with a bus identity) and real `st_rdev` numbers; without a bus in sysfs `drmGetDevices2` silently drops card0. That plumbing lives in the VFS — see [VFS](../servers/vfs.md).

## How it works

### Mode setting and EDID

The KMS driver autodetects the native display resolution through **EDID**: it reads the EDID block from the virtio-gpu device, validates header and checksum, parses the first detailed timing descriptor for preferred width, height and refresh, and configures the framebuffer accordingly. No display size is hard-coded, so the kernel configures itself correctly on first boot across display sizes (1920×1080 on the x86_64/OVMF path, 1280×800 on the usual aarch64 run).

`GETCONNECTOR` reports the native mode first (`PREFERRED|DRIVER`) followed by the standard sizes that fit inside it, each at exactly 60.000 Hz, with a physical size computed at 100 DPI (QEMU's EDID convention). Before 2026-10-02 it listed one mode with a refresh computed from an integer clock (59.99x Hz) and a fixed 320×200 mm, so COSMIC Settings had nothing to switch to; `cosmic-randr` mode and scale changes are now verified on both architectures (`59057ac` carries an atomic `MODE_ID` into the CRTC mode).

### Object model and properties

One CRTC, one connector, one encoder, a primary plane and a cursor plane, each with a full property tree (`OBJ_GETPROPERTIES`, `GETPROPERTY`, property blobs). Two constraints from consumers shape it:

- drm-rs indexes `values[0]`/`values[1]` of RANGE/SIGNED_RANGE/OBJECT properties unchecked, so a property reporting `count_values = 0` panics the compositor.
- `GETPROPERTY` is a two-pass ioctl and drm-ffi `set_len`s from the second call's count, so both passes must return identical counts.

### Legacy and atomic KMS

- **Legacy** — `SETCRTC`, `PAGE_FLIP`, `ADDFB`, `RMFB`, `DIRTYFB`. `SETCRTC` presents the framebuffer immediately, so a mode-setting compositor's frame 0 actually appears.
- **Atomic** — `DRM_CLIENT_CAP_ATOMIC` and `DRM_IOCTL_MODE_ATOMIC`, including `TEST_ONLY` and `ALLOW_MODESET`, with the legacy handlers still live for clients that do not opt in. Atomic is the preferred path because the legacy path cannot drive a cursor plane.

**Why atomic was the real gate.** The ~1 fps pointer on the softpipe desktop (measured 0.9 page flips/s under 60 pointer moves/s, kernel flip path only 1.7 ms of a ~1.1 s frame) was a missing cursor plane. Reading smithay's source settled that no kernel-side cursor plane could help while `DRM_CLIENT_CAP_ATOMIC` was refused: its DRM compositor clears cursor and overlay planes on legacy surfaces, and its only legacy `set_cursor` is a teardown. Atomic KMS (`dc3419b`) plus a real cursor plane (`d15a657`) took the desktop from 0.9 to 6.0 flips/s, with the cursor image uploaded once and every later pointer update a bare `MOVE_CURSOR`. An in-kernel cursor composite was rejected: it would recover ~0.15% of a frame and show two cursors.

A configuration trap hid this for a while: `COSMIC_DISABLE_DIRECT_SCANOUT=1` clears smithay's `ALLOW_SCANOUT`, a union that *includes* cursor-plane scanout, so the plane silently never engaged. The launchers set `COSMIC_DISABLE_OVERLAY_SCANOUT=1` instead, which expresses the actual intent.

### VRR properties

The CRTC carries `VRR_ENABLED` (0..1, mutable) and the connector `vrr_capable` (immutable, always 0). They exist because compositors probe them: libdrm and smithay look for `vrr_capable` to decide whether to offer adaptive sync. `VRR_ENABLED` is stored and read back the way Linux's core does (the core accepts a 1 on a non-capable sink; drivers merely leave adaptive sync off), through both atomic commits and the legacy `DRM_IOCTL_MODE_OBJ_SETPROPERTY` (master only; any other property, a value above 1 or a write to the immutable property is `EINVAL`). Nothing programs variable refresh: virtio-gpu has no such scanout and i915 does not yet parse the EDID range descriptor or DP `MSA_TIMING_PAR_IGNORED`. `drmsmoke` guards the round trip.

### sysfs view of the connectors

libdrm, udev and cosmic-monitor read DRM state from sysfs, so [`servers/vfs/src/sysdrm.rs`](../../servers/vfs/src/sysdrm.rs) generates `/sys/class/drm/card0-<TYPE>-<n>/{status,enabled,modes,dpms,edid}` from the same live connector state `GETCONNECTOR` answers (the DRM stack registers a hook table, since the VFS cannot depend on the drivers). The same directories answer under `/sys/devices/.../drm/card0/`. The overlay is deliberately narrow: only `card0-*` connector paths are claimed, because an earlier overlay that shadowed all of `/sys` blacked out the desktop. The rest of the tree (class directory, `card0`/`renderD128` links, `/sys/dev/char/226:*`, `device/*`) is staged on disk by `scripts/mkfs-f2fs-populated.py` Linux-shaped: real directories under `/sys/devices/pci.../drm/cardN` with the class and dev-char entries as symlinks into them, so `realpath` and udev's parent walk behave. Listings of the class directory do not show the generated connectors; lookups by name do.

**EDID in sysfs.** The connector's `edid` file returns the same bytes as the immutable `EDID` blob property (lane sysedid, 2026-11-19). The virtio-gpu EDID is fetched lazily, so the x86_64 path (virtio-vga with the GOP framebuffer, which never runs the console-framebuffer setup that used to fetch it) fills it too (lane edidx86). The i915 extra connectors serve their DDC/AUX EDID the same way ([below](#the-i915-backend)). The PCI directory of the DRM device is staged with `device`, `driver`, `irq`, `resource` and `modalias` entries so a sandboxed process (Firefox's RDD broker for the VA-API driver) is allowed to read what it expects. Every synthesised sysfs directory also has a real fd path (`readlink`, `openat`, bind or `open_tree` mounts as bwrap does), and `/sys/class/block` no longer resolves to the ACPI tables directory (lane sysfsbind).

### Planes and the cursor

The primary plane is the scanout; the **cursor plane** is driven from the virtio-gpu cursor queue. Before it existed, every pointer movement forced a full-screen software recomposite; now moving the pointer costs a cursor-queue update. QEMU hard-caps the cursor at exactly 64×64 and silently drops other sizes, which `DRM_CAP_CURSOR_WIDTH/HEIGHT` report. A working hardware cursor disappears from QMP `screendump` (QEMU keeps it off the display surface) — that is evidence of success, not a regression.

### Framebuffers and presenting

`ADDFB`/`ADDFB2` create framebuffers over dumb buffers, virgl 3D resources or blob buffers, and a present takes one of two shapes:

- **CPU present** — for dumb and guest-backed buffers, copy into the console's resource 1 and flush. `perform_software_scaling` ([`drivers/src/drm/device.rs`](../../drivers/src/drm/device.rs)) copies directly when sizes match and otherwise scales **nearest-neighbour**, which lets a fixed-resolution client (DOOM) fill a high-resolution display without layout changes.
- **Zero-copy present** — for a blob BO that lives in host memory (every buffer Zink renders) or a virgl 3D resource, the framebuffer records the resource and a present points the scanout at it with `SET_SCANOUT_BLOB`/`SET_SCANOUT` plus `RESOURCE_FLUSH`. That is upstream's `virtio_gpu_primary_plane_update` model and the whole point of rendering on the host GPU. Before `5c3c552` there was no scanout path for host-memory buffers at all: `ADDFB2` built a 2D resource over guest address 0, QEMU refused the attach (`ctrl 0x106, error 0x1200`), and cosmic-comp flipped at ~20 fps into a black screen. "Flipping at N fps into black" now reads, diagnostically, as "KMS accepted a framebuffer it cannot present".

Since `b1d9975` framebuffers hold a BO reference and die with their open: `RMFB` refuses another open's framebuffer, unlinks it from planes, and if the host is scanning it out sends `SET_SCANOUT res 0` first; `drm_release_open` sweeps an open's framebuffers before its handles, mirroring `drm_fb_release`. Before that a framebuffer could outlive its BO's host resource, which showed up as `SET_SCANOUT error 0x1203 (invalid resource id)` and a 2D present reading freed pages.

### Damage

`FB_DAMAGE_CLIPS` blobs are decoded and only the named sub-rectangles copied, with one flush over their bounding union (`c5abb8d`). Clips are clamped and degenerate ones dropped rather than rejecting the commit: a bad clip list is a hint the kernel is free to ignore, and failing the commit would stall the compositor.

Measured honestly, the win is small: smithay damages ~97% of the output on every present while anything moves and never skips the primary plane. A source analysis refuted the recorded explanation (buffer age 0 / per-frame reallocation): the age-0 fallback fires exactly twice during bring-up, and during an idle stretch damage is a precise 1280×32 strip (the panel clock). The near-full damage during motion is genuine element damage rounded *up* by smithay's `DamageShaper`. The `[DRMSTAT]` damage counters (behind `DRM_STATS`, off by default) exist to keep that honest.

### Page-flip events

Flip completions are not delivered at submission — that lets a compositor's render loop resubmit with zero delay and peg a CPU, since there is no real vblank. They queue in `PENDING_FLIPS` with the fence of the present they stand for, and are promoted to readable events by whichever comes first:

- the present's fence retiring, observed on the GPU completion interrupt (the host has shown the frame); or
- the 100 Hz `drm_tick` fallback, for unfenced commits (cursor-only) and for fenced ones unanswered within `FLIP_FALLBACK_NS` (two full tick periods, 20 ms).

Events are **per open**: a client killed with a flip in flight used to leave its event for the next client, whose first `read()` returned the wrong `user_data` and every later read one event behind. The fallback deadline is measured with `arch_monotonic_ns()`, not tick counts — a tick counter sampled at an arbitrary phase let "two ticks" elapse after one real tick period, and real virgl presents at 11–16 ms lost that race intermittently (the same class as the `FUTEX_WAIT_BITSET` and timerfd bugs; see [Time](../kernel/time.md)). With the completion interrupt, flip-event latency on aarch64/HVF fell from 9.8 ms mean to 1.3–1.75 ms and the presentable flip ceiling rose from 101 to 570–740 per second.

### PRIME and dmabuf

`PRIME_HANDLE_TO_FD` / `FD_TO_HANDLE` export buffers as borrowed VMOs in the VFS, with `DRM_CAP_PRIME` reported so GBM takes the DRIimage path. The export is correct for multithreaded clients, and the ephemeral export node is unlinked so it cannot leak the tmpfs pool. Venus **blob** handles export through the same path.

**Imports mint the importer its own handle.** Handles are scoped per open file description, as upstream scopes them per `drm_file`. Two bugs came from echoing the exporter's handle instead:

- Blob BOs (`51d9248`) — the importer's `RESOURCE_INFO` was refused; one of the three fixes behind the first real Zink frames.
- Dumb and virgl-3D BOs (`4544e3d`) — Mesa's virgl winsys imports every client dmabuf and closes the handle when the `wl_buffer` dies, which retired the *exporter's* handle. Every later import landed on a dead handle; cosmic-comp posted `invalid_wl_buffer` ("Protocol error 7") and the panel never appeared under virgl. Each importing open now gets an alias handle holding one reference on the primary record, deduplicated per (object, open).

### Buffer-object lifetime

A BO is destroyed when its reference count reaches zero. References are held by each GEM handle naming it and by each **exporting VMO slot** in the VFS — per slot rather than per fd, because `dup`/`fork`/`SCM_RIGHTS` copies of one dmabuf fd share one slot, which is destroyed exactly once. Before this rule, `GEM_CLOSE` after export freed the frames while the dmabuf fd still named them, and a `read()` returned whatever the buddy allocator had since handed out — page tables, slab, another process's memory — with no panic, because the frames stay HHDM-mapped and merely belong to someone else.

The opposite failure, double release, corrupts the allocator rather than leaking, so two structural guards exist: every decrement is a test-and-remove under one acquisition of the object map, and the teardown lives inside the zero arm of the unref function, so there is no `release(record)` entry point for a caller to misuse. An underflow logs `[DRM] bo refcount underflow` and frees nothing. The handle and object maps are separate leaf locks taken one at a time; `VIRTIO_GPU` is never taken with a BO map held.

### Per-open state

State that upstream scopes per open file is scoped per open file here: the virtio-gpu 3D context, GEM handle ownership, fences, flip events and framebuffers are keyed by the VFS's per-open cookie (`open_id`), refcounted across `dup`/`dup2`/`dup3`/`F_DUPFD`/`fork`/`SCM_RIGHTS`. That is what lets a multithreaded compositor and a Vulkan client hold the device at the same time. The original Venus transport had one process-global context, so `CONTEXT_INIT` tore down whoever came before (`997a45e` fixed it). A scan of the fd tables was rejected as the keying mechanism because an fd in flight through `SCM_RIGHTS` is in no table.

Ownership is an owner tag on a global handle space: a caller may reach a BO it owns, an unowned one, or any if it has no identity (the legacy path). Everything else is a miss indistinguishable from a never-allocated handle, so one client cannot probe for another's buffers. **Dumb buffers are deliberately not scoped**: `ADDFB` and the console path consume them with no open identity, and scoping them would break the compositor for no gain. Up to 256 per-open GPU contexts exist (raised from 16, which capped a desktop at about ten wgpu terminals with no error message).

An NVIDIA sysmem mmap is checked in two steps (`drivers/nvidia/src/mmap_tokens.rs`): the mmap ioctl, which sees the address and the open, checks ownership of the buffer containing the address and records an authorization tagged with that buffer's generation; the page pin, which sees the length, requires the whole `[addr, addr + len)` inside that same live buffer (page-aligned, non-empty, no overflow). A buffer freed, or freed and its pages reused, between the two steps is refused rather than falling back to an unpinned device mapping. Host-tested in `drivers/uapi-tests` together with the pin refcounts of `mm/src/pageref.rs`.

### Fences

- `EXECBUFFER` with `FENCE_FD_OUT` mints a real eventfd, created at zero and signalled when the host retires the fence. The fd is reserved **before** submission; on `EMFILE`/`ENFILE` nothing is submitted and `fence_fd = -1`, which is upstream semantics. Mesa's virgl winsys then discards the command stream ("expect bad rendering"), the host sees a later reference to an object that never arrived, and that context dies — exactly as on Linux. The defence is fd headroom (512 per process); the refusal is logged once per power of two.
- Each BO carries a `last_fence`, attached from `EXECBUFFER`'s `bo_handles`; `VIRTGPU_WAIT` waits on the fence of the BO it names. An unknown handle fails the whole submission with `ENOENT` before the device is touched. The per-open last fence is deliberately *not* a WAIT fallback — that would recreate the wrong-fence coupling the per-BO model removed.
- Retirement is published as a lock-free floor; an out-of-order retirement signals when the floor passes it — late by one older submission, never early.

### Sync objects

`DRM_IOCTL_SYNCOBJ_*` (create, destroy, handle/fd conversion, wait, reset, signal, timeline variants, query, transfer, eventfd) are implemented with real fences and errno values Mesa branches on (`ETIME` vs `ENOENT` vs `EINVAL`). Virtgpu's Mesa paths can simulate syncobjs, but v3d's `drm_v3d_submit_cl` passes them unconditionally, so this is a prerequisite for the Raspberry Pi 5 GPU that is fully testable under QEMU. Retirement is a lock-free watermark so it can be called from a completion ISR.

Two additions came with the NVIDIA bring-up. `syncobj_core` models a payload as Linux's fence chain (binary or timeline, `WAIT_FOR_SUBMIT`/`WAIT_AVAILABLE`, prefix lookup for `TRANSFER`), and `TIMELINE_WAIT`, `QUERY`, `TIMELINE_SIGNAL` and `TRANSFER` are answered while a GPU backend is registered; `WAIT` accepts the 40-byte struct current libdrm sends. **Sync files** — `SYNCOBJ_HANDLE_TO_FD`/`FD_TO_HANDLE` with the sync-file flags, `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`/`IMPORT_SYNC_FILE` (per-object reservation fences, i.e. dma-buf implicit sync), `DMA_BUF_IOCTL_SYNC`, `SYNC_IOC_MERGE` and `SYNC_IOC_FILE_INFO` — are an eventfd signalled from the DRM tick when the fence retires; pure semantics are in `sync_file_core.rs` (host-tested), and the whole family is gated on a GPU UAPI backend (or `drm.syncfile=1`), so the virtio paths answer exactly as before.

### Driver identity

`DRM_IOCTL_VERSION` reports `virtio_gpu` 0.1.0 on both nodes. card0 used to report `leandros-drm` on purpose, so Mesa's loader would fall through to software while the megadriver was softpipe-only; once virgl was in the megadriver that same choice made Mesa report `renderer: "softpipe"` against a working virglrenderer host. The render node must say `virtio_gpu` at major 0 or Venus refuses it. When the v3d backend is armed, both nodes report `v3d` instead (a single-device design; see [mesa.md](mesa.md#v3d-for-the-raspberry-pi-5)).

### GPU backends: `gpu_uapi` and `gpu_kms`

Two seams let a driver in another crate own what the DRM server would otherwise answer itself. **`gpu_uapi`** (lane nvuapi) lets a backend own the device identity, the driver-private ioctl block, `GEM_CLOSE`, mmap tokens, PRIME and per-open teardown, and the PCI identity the VFS serves for the DRM nodes' sysfs files; `DriverError::Errno` carries exact errnos, and `/proc/cmdline` is readable so launchers can see the options. The NVIDIA backend (`drivers/nvidia/src/uapi.rs`) registers as `nouveau` 1.4.0 ([pc.md](../hardware/pc.md#stage-m5m6-the-nouveau-uapi-and-nvk)). **`gpu_kms`** (lane nvkms) is a second, separate seam for scanning out framebuffers a GPU backend owns: wrap a backend GEM handle in a framebuffer (`ADDFB`/`ADDFB2`), present it, destroy it. The synthetic connector, CRTC and plane, the atomic properties, flip events, DRM master and the console hand-off are reused unchanged; presents of backend framebuffers run with no DRM lock held and queue an unfenced event, delivered by an hrtimer at a 60 Hz synthetic vblank (per-frame and fence-wait counters, a `[nvkms]` frames line every 5 s while presenting). While a KMS backend is registered the connector lists only its fixed mode and other modes are refused, the primary plane gets `IN_FORMATS`, `ADDFB2_MODIFIERS` is 1, the cursor plane is hidden, and a VT switch landing during a backend copy repaints the text VT. Unregistered, every path is as before. `nvkmstest` reports SKIP when another client holds DRM master ([pc.md](../hardware/pc.md#stage-m8-kms-by-copy-engine-blits)).

### The v3d backend

On a Raspberry Pi 5 the v3d UAPI is backed by a real driver (`drivers/src/v3d/`, submit queue, MMU, IRQ; see [hardware/raspberry-pi-5.md](../hardware/raspberry-pi-5.md#v3d-gpu)), reached through a `kenv` seam and a `KV3dOps` table so that `V3D` can be built in, a module or absent. Display is the single-device design: `ADDFB2` takes a v3d GEM handle directly, and [`v3d_scanout.rs`](../../drivers/src/v3d_scanout.rs) puts it on screen through the HVS (zero copy), a stride copy into the firmware framebuffer, or a headless RAM surface; see [V3D scanout and the HVS](../hardware/raspberry-pi-5.md#v3d-scanout-and-the-hvs). Page-flip events complete when the HVS has latched the new list, with the vblank phase predicted from observed latches. The rest of this section describes the **stub** backend that runs everywhere else (QEMU models no V3D) and that keeps the UAPI decode testable without a board:

[`drm_device_interface.rs`](../../drivers/src/drm_device_interface.rs) carries the V3D 7.1 UAPI as a stub backend: `GET_PARAM` (a fixed, self-consistent V3D 7.1 identity with `MAX_PERF_COUNTERS = 0`), `CREATE_BO`, `MMAP_BO`, `GET_BO_OFFSET`, `WAIT_BO`, `GEM_CLOSE`, and `SUBMIT_CL`'s decode — BO-list validation, in-sync resolution, fence allocation, deferred retirement. There is no GPU behind it: QEMU models no V3D, so the command lists are not even read and `SUBMIT_TFU`/`SUBMIT_CSD`/`PERFMON_*` are `ENOSYS`. What it closes is the half of a GPU driver that is pure decode and is expensive to debug with a board on the desk.

v3d ioctl numbers collide bit-for-bit with virtgpu's (`V3D_WAIT_BO == VIRTGPU_MAP`, `V3D_MMAP_BO == VIRTGPU_GETPARAM`), because DRM numbers are per driver. The v3d arms therefore sit in a separate dispatch ahead of the virtgpu `match`, gated by one atomic armed through `DRM_IOCTL_SET_CLIENT_CAP` with private capability `0x10000003` — an existing entry point that already ignores unknown ids and can be toggled within one boot, which is what made A/B evidence possible (`drmsmoke --arm-v3d` / `--disarm-v3d`). Default off; disarmed it is one relaxed load. Nothing auto-detects it: "there is no virtio-GPU" is the wrong proxy (also true of headless QEMU).

### The i915 backend

On the laptop (Intel Gen9.5, see [hardware/pc.md](../hardware/pc.md#hp-elitebook-840-g5-laptop)) `drivers/src/i915/` plugs into the same `gpu_uapi`/`gpu_kms` seams: it answers the i915 UAPI for Mesa iris and ANV (GEM, execbuffer2 with relocations, `MMAP_OFFSET`, a PPGTT per context) and provides KMS for the eDP panel. QEMU cannot emulate an Intel GPU, so `i915.fake=1|2` with `i915.kms=1` serves fake registers to run the decode, bring-up and `drmsmoke` paths; real behaviour is only provable on the laptop. The display-specific pieces:

- **Scanout memory and flips.** `I915_GEM_MMAP_OFFSET` hands out one opaque offset per (BO, caching mode); WC and UC requests map with write-combining PTEs (x86 PAT WC), and the legacy `GEM_MMAP` goes through the same path (`i915wc`, `i915ddb`). A flip flushes the cache (`clflush`) only for BOs that are bound cached, have a CPU write-back mapping or had a CPU write since the last flip (`i915.kms_clflush=2`, default; `=3` is the old always-flush-PRIME rule). cosmic-comp PRIME-exports GBM buffers that Mesa marks uncached, so its cost was already zero. Flips are dirty-tracked and complete on real vblanks; `/proc/i915` carries a FIFO underrun counter.
- **Watermarks and DDB.** Watermarks come from the PCODE latencies (`skl_compute_wm`). With pipe A the only lit pipe, the display data buffer is split the way Linux does: plane 1A gets blocks 0..860 and the cursor 860..892 of the 892 usable blocks, and the plane and cursor watermarks are recomputed; if another pipe is lit the firmware allocation is kept, and since `lapdispC2a` the arithmetic (`i915/ddb.rs`) is a proportional split across lit pipes. `i915.ddb=0` restores the old behaviour; `i915.ddb_test=1` shrinks plane 1A for ten seconds while counting underruns.
- **External outputs** (`i915/outputs.rs`, lanes `lapdisp`, `lapdispA`). The external DDI ports are read from the VBT child devices (DDC pin, AUX channel, type), falling back to `SFUSE_STRAP`; the laptop has DDI-B and DDI-C, both DP++, so each becomes a DP connector and an HDMI-A connector as on Linux. Live presence comes from `SDEISR`; native DP AUX (`DP_AUX_CH_CTL` at `0x64010 + ch*0x100`, power well 2 held for the transaction, defer/timeout retries) reads the EDID over I2C-over-AUX, HDMI reads it over GMBUS; EDID detailed timings become DRM modes. The extra connectors have ids `0x100+i` and encoders `0x180+i`; while any is registered connector 1 is reported as eDP (type 14, it used to be 11). **Default off**: `i915.extout=1` enables it, `i915.extout_aux=0` skips the boot AUX probe, `i915.extout_show=1` also shows connectors that have no CRTC (they are hidden as disconnected otherwise). It stays off because a trial meant to check that cosmic-comp tolerates the extras found the laptop unreachable before deployment, so compositor health with them is unverified.
- **Hot-plug** (`lapdispB`). With `extout`, the PCH hotplug interrupt (`SDEIMR`/`SDEIER` bits 21..23, DDI B..D, only once the display MSI is live) is acknowledged in the ISR and handed lock-free to the `i915-hpd` kernel thread, which debounces (`i915.hpd_debounce_ms`, 100), re-probes, and on a change of connection or modes sends a DRM `change` uevent: evdev pushes `change card0 drm` on `/dev/input/hotplug` and the libudev shim turns a DRM-only monitor into a `card0` record with `HOTPLUG=1`. A short pulse is only logged; `i915.hpd=0` leaves a 2 s poll of the live bit. A 300 s `hpd stats:` line summarises it. `kexec_irq_off` masks the SDE interrupts too. `drmsmoke --hotplug [--fake]` waits up to 60 s for the record; a root-only ioctl `0x100B` (`DRM_DEBUG_FAKE_HPD`) fakes one.
- **A second CRTC** (`lapdispC1`). The DRM core is no longer single-CRTC: `KmsPipe2` is the second pipe's backend seam (`register_pipe2`), CRTC 2 has plane 32 and cursor plane 33, `GETRESOURCES`, `GETPLANE`, `GETENCODER` and atomic commits route per CRTC, and validation refuses a plane on another pipe's CRTC, more than one connector per CRTC, an active CRTC without a connector and a valid mode blob, and eDP on CRTC 2. A commit touching only B skips pipe A's busy check, and flip events carry `crtc_id=2`. Everything is gated on a registered pipe B; the only implementation so far is the test stub (`i915.pipeb_stub=1`, 60 Hz flips from a kernel thread), covered by `drmsmoke --crtc2` and the `i915crtc2` runtests suite.
- **Pipe B prerequisites** (`lapdispC2a`, nothing lights a display). `i915/power.rs` refcounts power well 2 and the DDI IO wells (a well the BIOS requested is never released), `wrpll.rs` is Linux's `skl_ddi_calculate_wrpll` and CFGCR packing, `buf_trans.rs` the Skylake/KBL-U HDMI and DP voltage-swing tables (the HDMI default is table entry 8, written to hardware slot 9). `i915.pipeb_test=pll` locks DPLL for 148.5 MHz and releases it; `=dryrun` also logs the whole HDMI register plan as `pipeb: plan 0x<reg> <- 0x<val>` without writing; `=hdmi1|hdmi2` is reserved for the light-up. `i915.pipeb_port` (default 1, DDI-B) and `i915.pipeb_dpll` choose resources.
- **Cursors, async flips, vblank waits, in-fences** (lane `i915feat`). Cursors of 64, 128 and 256 ARGB are accepted (`DRM_CAP_CURSOR_WIDTH/HEIGHT` report 256; `/proc/i915` shows `cursor_size`). `DRM_MODE_PAGE_FLIP_ASYNC` (atomic flag and legacy flip) sets PLANE_CTL's async-flip bit with the new `PLANE_SURF`, so the surface changes at once instead of at the next vblank; combining it with `ALLOW_MODESET` is `EINVAL`, and `DRM_CAP_ASYNC_PAGE_FLIP` is 1. `DRM_IOCTL_WAIT_VBLANK` and `CRTC_GET_SEQUENCE` read the hardware `PIPE_FRMCOUNT` (the wait polls it every 500 us rather than unmasking an interrupt while nothing flips). A plane's `IN_FENCE_FD` is resolved in `kernel/src/syscall.rs`, where the fd table lives, and CPU-waited (interruptibly) before the driver sees the commit; a value that is not a fence fd is `EINVAL`, -1 means none. On the QEMU fake device the frame counter used to move only while flips were pending, so it now counts 60 Hz in time. Verified on the laptop (`i915test`, 23 new `kms_*` cases). Open: `WAIT_VBLANK` is polled rather than interrupt-driven, `VBLANK_EVENT`/`SIGNAL` are `EINVAL`, and a non-blocking commit blocks on an unsignalled in-fence.

Open: nothing drives a second display yet (the light-up step is planned, not landed), and no monitor-attached test has run; see the open items in the laptop section.

### DRM master, one per VT

Master is per open of card0 and decides who may move pixels into the shared surface. The gated ioctls are derived, not guessed: every path that writes the scanout bumps `SCANOUT_WRITES`, and those sites are reachable only from `SETCRTC`, `PAGE_FLIP`, `ATOMIC`, `DIRTYFB` and the custom codes `0x1001`/`0x1004`/`0x1005`. Exactly that set is refused with `EACCES` for non-masters, before the dispatch `match`, so a refused ioctl can never claim the scanout. No render ioctl can reach it, so Vulkan clients are untouched.

Master became a real grant with VT-scoped suspension in `959710d`. A grant remembers the VT that was on screen when it was made and is armed only while that VT is foreground. Suspension is derived from the active VT rather than written on a switch, so a switch cannot race a present; switching back re-arms it without the client re-issuing `SET_MASTER`, because a compositor that does not re-ask would otherwise return to a permanently dead display. There is **one grant per VT** (`MASTER_BY_VT`, `869014d`): with a single device-wide slot a KMS client on a text VT (DOOM) took the slot while COSMIC was backgrounded, and COSMIC's `SET_MASTER` was `EBUSY` on return. That is logind's "the foreground session is master" rule with the revocation derived instead of performed. An open that held and dropped master is not re-granted implicitly by its next present, as on Linux. Legacy authentication (`GET_MAGIC`/`AUTH_MAGIC` tokens, [`drivers/src/drm/auth.rs`](../../drivers/src/drm/auth.rs)) is answered for clients that still use it; render-node clients skip it. Measured: cosmic-comp drops and retakes master itself on every switch, so the libseat shim stays passive. See [TTY and VT](../servers/tty-and-vt.md).

**Out-fence flush.** After a CPU wait on a fence (`FENCE_FD_IN`) the kernel now signals every out-fence fd whose fence the floor has already passed (`out_fence_flush`), instead of waiting for the next `gpu_fence_event` tick. On Venus the host retires work instantly, so the fd could still read "pending" after the wait that proved it done. A second hole was a lost wake: `out_fence_service` skips the queue when it reads the out-fence count as zero, so a retirement that ran between a submitter's floor check and its count store left the fence stranded until the next unrelated event (an aarch64 greeter hang, lane blanksess2). The count store is now sequentially consistent and the queue is re-checked after it. `/proc/pollfds` (try-lock only, never blocks) shows the GPU fence floor, the out-fence queue (each entry with its retired flag), the waiter counts and every open DRM context's last fence, for diagnosing such a hang.

### LeandrOS-private ioctls

The original DOOM path uses custom codes `0x1001`–`0x1007` (set mode, create framebuffer, flip, mmap); `0x1008` returns the GPU interrupt census and `0x1009` (root only) sets the control-queue spin-before-park. Private `GETPARAM`s expose diagnostic counters (live dumb objects, framebuffer sweep statistics, the per-open last fence). `DRM_IOCTL_VIRTGPU_MAP` is backed by `map_kernel_device`, so userspace maps buffers directly and display updates bypass the VFS write path — the "non-blocking DRM mmap" decision that made full-frame rendering throughput possible.

## Design decisions

| Decision | Why | Rejected alternative |
|---|---|---|
| Extend the live UAPI handler, not the old `drm_driver.rs` | The latter used a bespoke wire format with invented command numbers; only Linux UAPI is useful to Mesa | Reusing the "richer" dormant driver |
| Derive ioctl request codes from `_IOC(dir,type,nr,size)` and compiled struct sizes | Two guessed codes cost whole sessions; the pre-Venus virtgpu constants omitted `DRM_COMMAND_BASE` and had stale sizes | Hand-copied constants |
| Gate master on what *writes the scanout*, counted | A hardcoded ioctl list once missed `MODE_ATOMIC`, the one path a real compositor drives | A hand-maintained list (kept only where a refusal must precede the call, and checked by the counter) |
| Per-open state keyed by VFS cookie | Matches `drm_file`; survives fd passing | Per-process state; fd-table scans |
| Clamp bad damage clips | A hint must not stall the compositor | Rejecting the commit |
| Deliver flip events on the fence, tick as fallback | Gives a stable cadence, idle CPU at zero (`idletest` guards it) | Delivering at submit |

## Limitations and open work

- Only nearest-neighbour software scaling exists; there is no bilinear mode.
- Multi-head: the DRM core routes two CRTCs and extra connectors, but only virtio-gpu's second head (Venus mode) and the i915 test stub implement the second pipe; real i915 external outputs are detected, not lit ([above](#the-i915-backend)).
- `EXECBUFFER` out-fence fd exhaustion is still fatal to a virgl client's context, as on Linux. "Submit, then fail the fence" would be survivable but needs a review of Mesa's fence-NULL paths.
- The stub v3d backend described above executes nothing. On the Raspberry Pi 5 a real V3D driver exists separately and Mesa's `v3d` gallium driver renders GLES and Vulkan (`scripts/pi-v3d-sysfs.sh` fixes the card's sysfs identity); scanout works through the HVS on a real monitor (2026-11-25). Its frame rate is unmeasured, and one flip event of 8 ms or more per run still shows in `/proc/leandros_hvs`. See [mesa.md](mesa.md#v3d-for-the-raspberry-pi-5) and [Raspberry Pi 5](../hardware/raspberry-pi-5.md).
- Damage tracking is honest but cannot help much while smithay damages nearly the whole output.

## Code and tests

- Code: [`drivers/src/drm_device_interface.rs`](../../drivers/src/drm_device_interface.rs), [`drivers/src/drm/`](../../drivers/src/drm/), [`drivers/src/kms.rs`](../../drivers/src/kms.rs), [`servers/drm/src/lib.rs`](../../servers/drm/src/lib.rs).
- [`userland/drmsmoke`](../../userland/drmsmoke/src/main.rs) — raw-ioctl smoke test against card0: a real atomic commit, the console/scanout handoff, flip timestamps and fence-path delivery, the interrupt census, framebuffer lifetime (`FB_HOLDS_BO_REF`, `FB_SWEPT_ON_CLOSE`, `FB_RMFB_SCANOUT_DISABLES_PLANE`), out-fence atomicity, PRIME import semantics, master gating. KMS cases SKIP with a diagnosis when a compositor holds master. `--hold` paints a checkable frame for capture.
- [`userland/venustest`](../../userland/venustest) — transport conformance including per-open GEM ownership, dmabuf/blob lifetime and cross-open blob import.
- [`userland/vttest`](../../userland/vttest) — `master_per_vt` and VT arbitration.
- [`userland/idletest`](../../userland/idletest) — guards that flip delivery does not busy-loop.

## See also

- [virtio-gpu.md](virtio-gpu.md) — the device underneath.
- [console.md](console.md) — the other user of the scanout.
- [zink-and-virgl.md](zink-and-virgl.md) — the render paths that drive this interface.
- [VFS](../servers/vfs.md) — synthetic sysfs, VMOs, per-open cookies.
- [K4 design record](../design/k4-drm-design.md).
