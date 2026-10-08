# virtio-gpu driver

The virtio-gpu driver ([`drivers/src/virtio_gpu.rs`](../../drivers/src/virtio_gpu.rs)) implements the virtio-gpu 2D and 3D protocol over the modern virtio-PCI transport, including virgl contexts, Venus contexts, blob resources mapped through the host-visible shared-memory window, and the cursor queue. Its control queue is asynchronous with a real completion interrupt on both architectures — the first device interrupts in the system.

## Overview

- Full virtio-PCI capability parsing (common, notify, ISR, device config, shared-memory) with 64-bit BAR support.
- Two virtqueues: the **control queue** and the **cursor queue**. There is no event queue; display-change notifications are not consumed.
- Feature negotiation of `VIRGL`, `EDID`, `RESOURCE_BLOB` and `CONTEXT_INIT`. Gates follow what was negotiated: `virtio-vga-gl` without Venus offers `VIRGL` and `CONTEXT_INIT` but not `RESOURCE_BLOB`, so the 3D gates check each feature separately rather than demanding all three.
- 2D and 3D resource creation, transfers to and from the host, backing attachment, scanout and flush, `SET_SCANOUT_BLOB`, context create/destroy/attach/detach, `SUBMIT_3D`, capset queries, blob create/map/unmap.
- Exposed to userspace through the DRM layer's virtgpu ioctls: `VIRTGPU_GETPARAM`, `GET_CAPS`, `CONTEXT_INIT`, `RESOURCE_CREATE`, `RESOURCE_CREATE_BLOB`, `RESOURCE_INFO`, `MAP`, `TRANSFER_TO_HOST`/`FROM_HOST`, `EXECBUFFER`, `WAIT`. See [drm-kms.md](drm-kms.md).

The command numbering follows upstream `virtio_gpu.h` exactly: 2D commands at `0x01xx`, 3D/context commands at `0x02xx`. An early version crammed 3D commands into the 2D range (so its "submit" was really `GET_CAPSET_INFO`); note that `RESOURCE_CREATE_BLOB` is `0x010c`, in the 2D block, while only `MAP_BLOB`/`UNMAP_BLOB` sit in the 3D block.

## How it works

### Asynchronous control queue

Presents, transfers, `SUBMIT_3D`, and context attach/detach are submitted asynchronously: the vCPU pays only the enqueue cost. Completions are reaped from the used ring **by descriptor-chain head**, because QEMU answers fenced commands out of order; an in-flight table indexed by head holds each chain's fence, buffers and whether a caller is waiting on it. Reaping happens from three places:

1. the next submission,
2. the 100 Hz tick (`ctrlq_tick`, which only `try_lock`s the device and never allocates, so it is safe in IRQ context on any CPU), and
3. the **completion interrupt**: MSI-X on x86_64 (`virtio_gpu_msix_isr`, vector allocated from [`drivers/src/irq.rs`](../../drivers/src/irq.rs)), and INTx over a GIC SPI on aarch64 (`virtio_gpu_intx_isr`).

Measured on the Zink desktop: control-queue time fell from 42% to 0.03% of wall clock and the frame rate rose from 19.3 to 20.1 fps (`b4d30f0`, `63fc1c2`). The tick reaper alone, without the interrupt, *dropped* the rate to 12.9 fps — the interrupt is the win, not the asynchrony. (A later clock audit found the x86_64 microsecond counter of that period ran about 4.5× fast on that host, so the absolute share was nearer 9%; the before/after ratio stands.)

**aarch64 INTx.** QEMU `virt` wires the PCIe host bridge's INTA..D to SPIs 3..6 (INTIDs 35..38), swizzled by slot; the GPU in slot 4 lands on INTID 35. The handler reads the ISR status byte first, because that is what deasserts the level line. Every other virtio-PCI function the kernel polls (block, net, sound, input) sets the PCI command INTx-disable bit, since they never read their ISR and an asserted line would stay asserted forever — the GPU shares its line with virtio-net. A **storm guard** masks the SPI and falls back to polling after 1024 consecutive deliveries with no GPU condition; it was exercised with a deliberately broken kernel and the system stayed up. An ITS/MSI path on GICv3 was not attempted.

**Reply-needing commands** (`GET_CAPSET`, `RESOURCE_CREATE_*`, `MAP_BLOB`, `CTX_CREATE`, and the KMS-init 2D resource) still wait for their answer, about 3% of traffic at session start and none in steady state. The wait is structured to cost as little as possible:

- A `GpuSync` handle enqueues under the device lock, **drops the lock for the wait**, and retakes it, so other CPUs can use the device meanwhile. Each waiter is tracked per chain and kicked by IPI when its completion is reaped.
- The waiter spins for `CTRLQ_SPIN_BEFORE_PARK_US` (200 µs) before parking in `wfi` / `sti; hlt; cli`. Parking at step 0 regressed the session-start tail to ~9 ms (one tick): the interrupt is routed to the bootstrap CPU only, and if that CPU was itself spinning with interrupts off for the lock the waiter held, the waiter slept until its own tick. With the spin first, only about 5 of ~130 waits reach the parked phase.
- The interrupt handler kicks every waiting CPU with a reschedule IPI when it could not take the lock itself, so a waiter parked on another CPU does not sleep to its own tick (mean sync wait 2.5 ms → 0.1–0.4 ms).

### Fences and the host-hang watchdog

Each fenced chain carries a fence id; retirement publishes a lock-free floor that the DRM layer uses for out-fences, `VIRTGPU_WAIT` and flip events. `EXECBUFFER` fences signal on real host completion rather than a synthesize-at-submit shortcut.

`fence_watchdog` handles a host that stops answering. If no fence has been answered for 10 s, the pending fences are written off and `[GPU] HOST GPU HUNG` is logged; waits then return and submissions fail fast instead of blocking forever, newer fences are written off after 1 s while the condition lasts, and the first real answer clears it. This exists because a lost host GL context (for example the macOS Metal watchdog killing a command buffer) makes QEMU's virgl stop retiring fences for good. Demonstrated by a test build of virglrenderer that drops every fence after 60 s: the guest shell stayed responsive and the affected clients failed instead of hanging. A matching host patch (`virglrenderer-1.3.0-retire-fences-on-context-loss.patch` in [`scripts/mac-qemu-gpu/patches`](../../scripts/mac-qemu-gpu/patches)) makes the host retire fences on context loss.

When a context is destroyed with fenced commands still in flight, the host never retires them. The driver drains for up to 200 ms before `CTX_DESTROY`, then marks what remains abandoned so one parked chain cannot become a permanent hole under the fence floor. One control-queue chain per abandoned fence stays parked host-side.

### Resource ids

Dumb buffers, virgl 3D resources and blobs all draw from one `alloc_resource_id()` space, monotonic and never reused. Three framebuffer paths once derived their host resource as `handle + 10` (to dodge the console's resource 1); the 3D allocator starts at 16, so gem handle 6 and the first 3D resource were the same host resource. That was invisible while dumb buffers (the softpipe desktop) and 3D resources (Venus) were never busy together, and it surfaced under Zink — which uses both — as the host refusing `RESOURCE_ATTACH_BACKING`, which the then-synchronous busy-spin turned into a wedged guest. It looked exactly like "Zink is unsupported" (`a32f53d`). Context ids `1..256` are recycled lowest-first after `CTX_DESTROY` is answered.

`VIRTGPU_RESOURCE_CREATE` allocates real guest backing from the buddy allocator, passes the caller's actual pipe parameters (target, bind, depth, array size, levels, samples, flags), attaches it, attaches the resource to the creator's context, and registers it so PRIME, `MAP_DUMB` and `ADDFB2` resolve it. It was once a stub that sent `RESOURCE_CREATE_3D` and returned the same id as both the GEM handle and the host resource id — no backing, no registry — so every dmabuf export failed and cosmic-comp could not configure a CRTC.

### Blob resources and the host-visible window

A `HOST3D` blob lives in host memory. The guest asks the host with `RESOURCE_MAP_BLOB` to place it at a guest-chosen offset inside the device's shared-memory region (`VIRTIO_PCI_CAP_SHARED_MEMORY_CFG`, `shmid` 1 = host-visible; 0 means undefined), then maps that guest-physical range. Choosing offsets is entirely the guest's job, so this is a real allocator:

- **First-fit over the live spans** with 64 KiB granularity. First-fit makes the space recyclable (a Vulkan app creating and destroying ~20 blobs reuses low offsets rather than walking the window); 64 KiB alignment suits any host page size — 4 KiB x86_64, 16 KiB Apple silicon, 64 KiB large-page arm64 — because QEMU adds the resource as a RAM-device subregion and an unaligned subregion cannot back a memory slot.
- Bounded by the advertised window (4 GiB with `hostmem=4G`) and 64 MiB per request.
- **The BAR is never mapped eagerly.** Mapping the whole 4 GiB window at probe, as the capability loop does for every other capability, blew up the page tables; only sub-ranges are mapped.

**Cacheability follows the host** (`18a7a9f`). `RESOURCE_MAP_BLOB` answers with a `map_info` cache type; a host-visible blob the host marks write-combining or uncached is host memory the guest has no coherence with, and is mapped with `PageFlags::WRITECOMBINE` (PAT WC on x86_64, Normal non-cacheable MAIR on aarch64) via a hint in the device's mmap reply. Mapping it write-back stalled `vkrender` on x86_64/KVM: Mesa's fence-feedback slot lives in the first `HOST_COHERENT` (not `HOST_CACHED`) memory type, the host asked for WC on exactly that resource, and a write-back alias served a stale line indefinitely while both TCG configurations — which model no guest cache — passed. `WRITECOMBINE` is deliberately distinct from `NOCACHE`, which on aarch64 is Device-nGnRnE and would fault on Mesa's first unaligned access. The Venus command ring is reported `CACHED` and stays write-back. Device VMAs are aliased across `fork` rather than copied (`ce383ed`, which fixed a panic). See [Memory](../kernel/memory.md).

### Cursor queue

The cursor queue carries `UPDATE_CURSOR` and `MOVE_CURSOR`. It is fire-and-forget — a single read-only descriptor with no response — and asks for no interrupts, so it cannot stall anyone, which is itself diagnostic: a freeze that follows cursor-only motion is not the GPU driver.

### One device, one console: `virtio-vga-gl`

On x86_64 OVMF and Limine need VGA registers to expose a GOP framebuffer, and `virtio-gpu-gl-pci` has none. Pairing it with q35's std-VGA left **two display consoles**: the text console on head 0, and everything cosmic-comp scanned out on head 1, while the window, VNC and `screendump` all showed head 0. A desktop compositing correctly on Vulkan was indistinguishable from one that never started, and that off-by-one-head is what made an early Vulkan presenting result "pixels unproven". `virtio-vga-gl` is virtio-vga plus a virglrenderer context and accepts `venus=on`, `blob=on` and `hostmem=` exactly like `virtio-gpu-gl-pci`, so it satisfies OVMF and Venus with a single device and `-vga none` intact (`8a2d16f`). The runner probes for `virtio-vga-gl` before `virtio-vga` — `grep virtio-vga` also matches the `-gl` name, which once made the GL branch dead code. aarch64 uses `virtio-gpu-pci`/`virtio-gpu-gl-pci`; VGA is an x86 concept.

### virgl presents

A virgl 3D framebuffer is presented with `SET_SCANOUT` of the 3D resource plus `RESOURCE_FLUSH`, never a 2D transfer. Transferring the guest pages over the host-rendered texture put stale staging memory on screen as noise (`d44b1e0` and its follow-up fixed it, together with the missing `CTX_ATTACH_RESOURCE` on resource creation and `RESOURCE_INFO` refusing virgl BOs).

## Design decisions

- **Asynchronous submission with a real interrupt rather than a faster spin.** The synchronous busy-spin cost 2,938 µs mean (worst 72.9 ms) per round trip under Venus against 11 µs under softpipe — the host is slow to answer once a GPU is behind it, and every caller spun the vCPU through all of it.
- **Hard failures, never silent ones.** On a plain `virtio-gpu-pci`, every layer refuses explicitly ("host does NOT offer VIRTIO_GPU_F_VIRGL", `GET_CAPS` → `EINVAL` rather than a buffer of zeros). The risk with a wire protocol is a silently wrong answer, so the transport test carries a gate asserting its own assertions ran.
- **IRQ-context code never blocks.** The tick and interrupt paths only `try_lock` and never allocate; work that loses the lock is retried next tick or by the woken waiter.

## Limitations and open work

- The parked wait for reply-needing commands, KMS init, the boot console, cursor `TRANSFER`s and ring-full waits still hold the device lock in places; moving every wait to the call sites is the remaining design step.
- The interrupt goes to the bootstrap CPU only; 1-of-N SPI routing (`GICD_IROUTER` bit 31) and MSI on aarch64 are untried.
- No event queue; display hot-plug is not handled.
- A `RING_IDX`-routed `SUBMIT_3D` whose stream the host refuses to dispatch is never completed: QEMU routes its completion through the renderer context, which never retires it. When submission was synchronous this cost a full control-queue timeout; now the fence simply stays pending until the hang watchdog writes it off, and a caller cannot tell it from a dead host. Real Mesa streams are valid protocol and never hit it; do not "fix" it by refusing or rewriting `RING_IDX`, which Mesa sets on every submit.

## Code and tests

- [`drivers/src/virtio_gpu.rs`](../../drivers/src/virtio_gpu.rs), [`drivers/src/irq.rs`](../../drivers/src/irq.rs), [`drivers/src/pci.rs`](../../drivers/src/pci.rs).
- [`userland/drmsmoke`](../../userland/drmsmoke/src/main.rs) — `GPU_IRQ_ARMED`, `GPU_IRQ_COMPLETIONS_COUNTED`, `FLIP_EVENT_DELIVERED_ON_FENCE`, `SYNC_CMD_PARKED`, `GPU_IRQ_NO_TIMEOUTS`, `INTX_NOT_STORMING`, driven by kernel counters so they are deterministic.
- [`userland/venustest`](../../userland/venustest) — capset, context, blob, map and scanout conformance.
- [`scripts/zinkbench.py`](../../scripts/zinkbench.py) and [`scripts/gpuirqbench.py`](../../scripts/gpuirqbench.py) — desktop frame rate and control-queue census (need `DRM_STATS = true`).

## See also

- [drm-kms.md](drm-kms.md), [vulkan-venus.md](vulkan-venus.md), [zink-and-virgl.md](zink-and-virgl.md).
- [x86_64](../kernel/arch-x86_64.md) (MSI-X, PAT) and [aarch64](../kernel/arch-aarch64.md) (GIC) architecture pages.
- [Virtual machines](../hardware/virtual-machines.md) — the QEMU device set.
