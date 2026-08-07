# Cross-open dmabuf import — execution-ready design

TODO item 8. Written 2026-08-06 against `a0f2c46` plus the landed PRIME export
(`e083202`). Design lane: nothing was built and no QEMU was run. Every claim below is
either a source citation, an arithmetic result, or is explicitly labelled as an
assumption with the measurement that settles it.

**Summary, three claims.**

1. **The ownership model.** Do not widen `open_may_reach`. Replace the flat
   `BLOB_BUFFERS` map with upstream DRM's two-level shape — a refcounted object plus a
   per-open handle that *is* one reference — and make `PRIME_FD_TO_HANDLE` **mint a new
   handle for the importing open** rather than hand back the exporter's. Authority
   becomes possession of the dmabuf fd, which is exactly the authority the fd table and
   SCM_RIGHTS already enforce (§2).
2. **Half of item 8 is a bug, not a feature.** An exported dmabuf fd does not keep its
   buffer alive today. `read()` on the fd after `GEM_CLOSE` returns recycled kernel
   memory, and `mmap(MAP_SHARED)` writes to it — a use-after-free reachable from **one
   unprivileged process, with no cross-open work at all**, pre-existing on the dumb path
   and widened to blobs by the export that just landed (§2.4). Stages 1–2 fix it and are
   due whatever is decided about M4.
3. **Item 8 is probably not the M4 gate, and M4 probably needs no kernel work.**
   cosmic-comp does not advertise `zwp_linux_dmabuf_v1` at all on a software EGL device
   (`kms/device.rs:760-761`), and there is no build flag to change that — only a
   cosmic-comp patch, which is forbidden. Meanwhile `MESA_VK_WSI_DEBUG=sw` — an
   environment variable, in a release Mesa — puts Venus's Wayland WSI on `wl_shm` with
   memfd-backed images, a path where every piece already works on LeandrOS. **Recommended
   route to M4: the environment variable, zero kernel days** (§6.2). One hour of
   measurement settles it (§5, Stage 0a).

---

## 1. What is actually in the way

### 1.1 The refusal, precisely

`open_may_reach` (`drivers/src/drm_device_interface.rs:1091`) is three terms:

```rust
fn open_may_reach(caller: u32, owner: u32) -> bool {
    caller == 0 || owner == 0 || caller == owner
}
```

`blob_lookup` (`:1097`) applies it to every blob BO consumer: `VIRTGPU_MAP` (`:3479`),
`RESOURCE_INFO` (`:3657`), `WAIT`, `GEM_CLOSE` via `free_blob_owned` (`:2607`), and
`EXECBUFFER`'s `bo_handles` via `bo_exists` (`:1108`) / `bo_attach_fence` (`:1129`). A compositor holding its
own `open_id` therefore cannot name a client's blob, which is correct and is `b80ab5a`'s
whole point.

Two things about the current state matter and are easy to miss:

* **Dumb buffers are not scoped at all.** `DUMB_BUFFERS` is consulted without any
  ownership test in `std_handle_map_dumb` (`:1946`), `std_handle_addfb` (`:1967`),
  `std_handle_addfb2` (`:2679`), `bo_exists` (`:1111`) and `dumb_buffer_phys_order`
  (`:1204`). The
  comment at `:1082-1090` says this is deliberate. So the compositor's *existing* GBM
  path already works cross-open — cross-open import is not a new capability in general,
  only for blob BOs.
* **`PRIME_FD_TO_HANDLE` today does not import anything.** `kernel/src/syscall.rs:6110`
  reads `TmpVmo.dmabuf_handle` back out and returns **the exporter's handle number
  verbatim**. It creates no per-open state. The number is globally meaningful, so a
  second open receives a handle that names the right object but which
  `open_may_reach` will then refuse for blobs — and will *not* refuse for dumb buffers.

That second point is the hinge of the whole design. The handle space is global; the
isolation is a tag comparison on top of it. Any correct cross-open story has to stop
sharing handle *numbers* across opens, because a shared number is a shared lifetime.

### 1.2 What a Wayland Vulkan client actually asks of the kernel

The M4 pipeline, step by step, with the kernel surface each step touches:

| # | Step | Kernel surface | Status |
|---|---|---|---|
| 1 | `vkCreateWaylandSurfaceKHR` | none (wayland-client) | works |
| 2 | swapchain image alloc → `vn_GetMemoryFdKHR` | `PRIME_HANDLE_TO_FD` on a blob | **shipped** (`e083202`) |
| 3 | `zwp_linux_buffer_params_v1.create` sends the fd | AF_UNIX SCM_RIGHTS | works (`scmtest` 30/0) |
| 4 | `vkQueuePresentKHR` → attach/commit | none | works |
| 5 | compositor `drmPrimeFDToHandle` on **its** card0 | cross-open import | **missing** |
| 6 | kms_swrast `MODE_MAP_DUMB` + `mmap` on the imported handle | `MAP_DUMB` accepting a blob handle | **missing** |
| 7 | compositor reads the pixels with the CPU | the blob must be CPU-readable | **probably not — §5.0b** |

Steps 5 and 6 are the deliverable. Step 7 is the risk, and it is not a plumbing risk:
it is a question about which memory type Mesa's WSI picks, and no amount of kernel work
changes the answer.

Step 3 has a precondition the table cannot express, and it turns out to dominate
everything: the compositor must *offer* `zwp_linux_dmabuf_v1` in the first place, and
cosmic-comp declines to on a software EGL device. See §6 — that finding, not any kernel
gap, is what decides this item.

Two corrections to the pipeline as item 8 states it, both found by reading Mesa 25.3.6
(`/Users/forain/code/leandros-artifacts/llvmpipe-lane/src/mesa`):

* **kms_swrast never issues `GEM_CLOSE` on an imported handle — it issues
  `DRM_IOCTL_MODE_DESTROY_DUMB`** (`src/gallium/winsys/sw/kms-dri/kms_dri_sw_winsys.c:288-296`,
  on the same field `add_from_prime` stored the prime handle into, `:409`). Our
  `std_handle_destroy_dumb` (`drivers/src/drm_device_interface.rs:2663`, dispatched at
  `:1443`) calls `Self::free_dumb(d.handle)` and nothing else, and takes no `open_id`.
  Left alone, every imported blob would leak one object **per composited frame**.
  `DESTROY_DUMB` must gain `open_id` and call `free_blob_owned` alongside `free_dumb`,
  exactly as `GEM_CLOSE` already does (`:2584`).
* **`MODE_MAP_DUMB` is issued on every map, not cached** (`kms_dri_sw_winsys.c:325`), so
  the blob branch in §3.4 is a hot path, not a one-off.

`ADDFB2` on blob handles, `SET_SCANOUT_BLOB` and the connector's missing `DPMS`
property appear nowhere in that table. They belong to `VK_KHR_display`, a different
consumer, and conflating the two is most of why item 8 reads as "several days". They
are **deferred** here (§4).

---

## 2. The ownership model

### 2.1 The candidate that must be rejected

Widening `open_may_reach` — by adding a "trusted compositor" open id, a global escape
hatch, or a per-BO "shareable" flag set at creation — fails on lifetime, not on access.
Access is only half the question the handle answers; the other half is *who may destroy
it*. With a shared handle number and a widened reachability test, the compositor's
handle-destroy reaches `free_blob_owned` → `release_blob` and tears down a buffer the
client is still rendering into. Mesa's kms_swrast importer destroys imported handles as a
matter of course — via `MODE_DESTROY_DUMB`, see §1.2 — every time a `pipe_resource` is
released. So widening turns a benign refusal into a cross-client destroy primitive fired
on the compositor's ordinary per-frame path.

### 2.2 The model: capability by fd possession, expressed as a per-open handle

Adopt upstream DRM's split, which exists for precisely this reason:

* **`BlobObj`** — the thing that has a host resource and guest pages. Refcounted.
  Nothing outside the registry ever names it.
* **`BLOB_BUFFERS: BTreeMap<u32 /*gem handle*/, BlobHandle>`** — unchanged key space,
  new value. A `BlobHandle` **is one reference** to a `BlobObj`, tagged with the
  `owner` open that may use it.

```rust
struct BlobObj {
    phys: usize,
    order: usize,
    res_handle: u32,
    size: u64,
    blob_mem: u32,
    last_fence: u64,
    // host-visible window bookkeeping — per RESOURCE, not per open
    win_off: u64,
    map_phys: u64,
    map_info: u32,
    /// live references: one per BlobHandle, one per exporting TmpVmo slot.
    refs: u32,
}
static BLOB_OBJS: Mutex<BTreeMap<u32, BlobObj>> = ...;   // keyed by obj_id
static NEXT_BLOB_OBJ: AtomicU32 = AtomicU32::new(1);

struct BlobHandle {
    obj: u32,
    /// the open that may use this handle. Unchanged semantics.
    owner: u32,
    /// the 3D context THIS open attached the resource to (0 = none).
    /// Moves off the object: attachment is per-open, so an importer with its
    /// own context attaches its own and detaches its own.
    ctx: u32,
}
```

`open_may_reach` is **not touched**. `blob_lookup(handle, open_id)` keeps its exact
signature and semantics; it simply resolves through two maps instead of one.

Who may reach a BO, on what authority:

1. **The creator**, by `RESOURCE_CREATE_BLOB` minting a handle with `owner = open_id`.
   Unchanged.
2. **Any open that presents a dmabuf fd naming the object**, by `PRIME_FD_TO_HANDLE`
   minting a *second, different* handle with `owner = that open`. The authority is
   possession of the fd.

Nothing else. In particular a handle number learned by guessing, by leaking, or by
reading another process's memory is still useless: `open_may_reach(caller, owner)` is
evaluated on the handle the caller names, and a handle minted for another open has that
open's `owner`. A malicious client's options are (a) guess a handle — refused, exactly
as today; (b) forge a dmabuf fd — it cannot, fds are minted only by
`PRIME_HANDLE_TO_FD` and transferred only by fork/dup/SCM_RIGHTS, all of which already
have their own access control; (c) receive an fd it was legitimately sent — which is
the intended grant, and is the same grant Wayland already makes.

Import must be **idempotent per (obj, open)**: a second `PRIME_FD_TO_HANDLE` of the same
fd on the same open returns the handle already minted and takes no second reference.
Upstream guarantees this (`drm_gem_prime_fd_to_handle` consults `file->prime.dmabufs`
first) and Mesa relies on it — kms_swrast caches imports by handle, so a non-idempotent
import both leaks references and breaks buffer identity comparisons in the compositor.

### 2.3 What the dmabuf fd has to carry

`TmpVmo.dmabuf_handle` (`servers/vfs/src/lib.rs:375`) currently stores a **gem handle**.
Under this model that is wrong: the gem handle is per-open and the importer's open must
not receive the exporter's. It has to store the **object id**, which is open-independent.

Minimal, compatible change: keep `dmabuf_handle` with its present meaning for the dumb
path (so `drmsmoke`'s three PRIME subtests are byte-identical), and add

```rust
/// Blob object id this dmabuf aliases; 0 = not a blob export.
/// Open-independent on purpose — the gem handle is per-open and must not
/// cross an fd.
dmabuf_obj: u32,
```

`PRIME_FD_TO_HANDLE` then dispatches: `dmabuf_obj != 0` → `blob_import(obj, open_id)`
returns a handle for *this* open; else `dmabuf_handle != 0` → today's dumb behaviour,
unchanged.

### 2.4 The reference that does not exist today, and the bug it already is

**This is the most important finding in this document, and it is not about M4.**

`release_blob` (`:2621`) ends with `mm::buddy::free(b.phys, b.order)`. `free_dumb`
(`:2052`) is the same two lines. Neither consults `mm::pageref`. Meanwhile
`vmo_free_slot` (`servers/vfs/src/lib.rs:443`) returns early for a borrowed VMO
*without* freeing, on the stated grounds that the DRM layer frees the block exactly
once.

That reasoning is sound in one direction and silent in the other. There is nothing
anywhere that makes the DRM object outlive the fd. So, at `HEAD` + `e083202`, from a
**single unprivileged process, with no cross-open work at all**:

```
h  = RESOURCE_CREATE_BLOB(blob_mem = GUEST, size = N)   // guest-backed → real pages
fd = PRIME_HANDLE_TO_FD(h)                              // borrowed VMO aliases them
GEM_CLOSE(h)            // release_blob → buddy::free(phys, order)
read(fd, buf, N)        // vmo_copy_out walks the freed frames through the HHDM
```

The `read` succeeds and returns whatever now occupies those frames — the buddy allocator
has already handed them out to page tables, slab pages, another process's anonymous
memory. `mmap(fd, MAP_SHARED)` is the same hazard with writes: **arbitrary kernel memory
corruption from an unprivileged process.** The identical sequence with
`CREATE_DUMB`/`DESTROY_DUMB` is **pre-existing**, independent of the blob export, and is
reachable by any process that can open `/dev/dri/card0`.

Cross-process, it is worse and it is the exact shape M4 would hit: client creates a
blob, exports, sends the fd to cosmic-comp over Wayland, then exits. `drm_release_open`
(`:1016`) reclaims every blob whose `owner` is that open — the note at `:1029-1037`
records that this reclamation was added deliberately — and the compositor is left with
an fd naming a freed object. Three distinct consequences, all realised:

* the host resource id is unref'd and virglrenderer may recycle it, so the compositor's
  next `RESOURCE_INFO`/transfer names **a different client's resource**;
* `hostvis_free(b.win_off)` returns the window span, so the next
  `RESOURCE_MAP_BLOB` lands at the same offset and the compositor's live mapping now
  aliases a stranger's resource — silent cross-client pixel disclosure;
* for a guest-backed blob, the buddy block is back in circulation under a live
  `MAP_SHARED` mapping.

**The rule.** A `BlobObj` is destroyed when its reference count reaches zero, and
references are held by:

1. each `BlobHandle` in `BLOB_BUFFERS` (any open), and
2. each **exporting `TmpVmo` slot**.

Granularity 2 is per *slot*, not per fd, and that is deliberate: `TMP_VMOS` is keyed by
the data-owning slot, so `dup`, `fork` and SCM_RIGHTS copies of one dmabuf fd already
share one slot (`servers/vfs/src/lib.rs:334-336`), and the slot is destroyed exactly
once, by `vmo_free_slot`. One ref per slot is therefore both sufficient and impossible
to double-drop.

`release_blob` moves behind `blob_unref` and runs only on the 1→0 transition. `DumbBuf`
gets the same `refs` field and `free_dumb` the same treatment — it is the same four
lines and it closes the pre-existing half of the hole.

**The failure if this is got wrong in the other direction** — double release — is the
class this project hit twice this week (`9be954f`, the `import_fd` EMFILE
double-release): two `resource_unref`s for one resource, and a **double
`mm::buddy::free` of an order-N block**, which is allocator corruption, not a leak.
The guard is that `blob_unref` does its test-and-remove under a single acquisition of
`BLOB_OBJS`, exactly as `free_blob_owned` (`:2607`) already does for the handle map, and
that `release_blob` is called only from `blob_unref`'s zero arm — never from a caller
that "knows" the count.

### 2.5 The layering problem, and how to not create a lock hazard

`vmo_free_slot` lives in `vfs-server`, whose `Cargo.toml` depends on `ipc`, `sched`,
`mm`, `xattr`, `spin` — **not on `drivers`**. It also runs with `TMP_FILES` held (its
doc comment says so, `:440-442`), and `release_blob` takes `VIRTIO_GPU.lock()` and
busy-spins on a device round-trip. Calling straight through would hold a tmpfs lock
across a device round-trip, and would put a second lock order (`TMP_FILES` →
`VIRTIO_GPU`) into a codebase that has one already.

The shape that avoids both:

* `vmo_free_slot` changes signature to `fn vmo_free_slot(owner: usize) -> Option<u32>`,
  returning the `dmabuf_obj` it just dropped (`None` for every non-blob slot). It takes
  no new lock and calls nothing.
* Its three call sites (`servers/vfs/src/lib.rs:2147`, `:2155`, `:2432`) capture that
  value, **let the `TMP_FILES` guard drop**, and only then call the release hook.
* The hook is a function pointer, because the dependency edge does not exist and must
  not be created: `static DMABUF_RELEASE: AtomicUsize` in `vfs-server`, plus
  `pub fn set_dmabuf_release(f: fn(u32))`, called once at boot from the kernel (which
  depends on both crates) with
  `drivers::drm_device_interface::blob_unref_exported`. A null pointer means "no DRM
  device" and is a no-op, which is the correct behaviour on a headless build.

This is the same discipline the file already states for `HOSTVIS_SPANS` and
`VIRTGPU_CTXS` ("never held across `VIRTIO_GPU.lock()`, never across user memory",
`:888-891`), and it keeps the project invariant that no user memory is touched under any
lock — the hook path touches none at all.

**Residual hazard, recorded not fixed.** `mm::buddy::free(phys, order)` frees an order-N
block outright and does not consult `mm::pageref` (`mm/src/pageref.rs:39-51` is the only
path that does). A process that `mmap`s its own dmabuf, closes the fd and then drops the
last handle still ends up with a live `MAP_SHARED` mapping of freed frames, because the
mapping's `pageref` is not a lifetime the DRM free path honours. This is unchanged by
this design and pre-existing on the dumb path. The cheap fix — free the block through a
pageref-aware path, or take an object reference in `vmo_acquire_frames` for borrowed
VMOs — belongs in its own item; it should not be smuggled into this one.

---

## 3. The minimal subset that unblocks M4

Four kernel changes, in dependency order. Everything else on item 8's requirement list
is `VK_KHR_display`, not M4 (§4).

### 3.1 Object/handle split with refcounting (§2.2)

Mechanical but wide: every `BLOB_BUFFERS` access resolves through the new pair. The call
sites are enumerable and few — `blob_lookup` (`:1097`), `bo_exists` (`:1108`),
`bo_fence` (`:1118`), `bo_attach_fence` (`:1129`), `handle_ioctl_mmap` (`:2794`),
`free_blob`/`free_blob_owned`/`release_blob` (`:2597`-`:2621`), `drm_release_open`
(`:1043`), `virtgpu_handle_resource_create_blob` (`:3407`), `virtgpu_handle_map`
(`:3479`), `hostvis_map_blob`'s record-back (`:3575`), `virtgpu_handle_resource_info`
(`:3657`). No externally visible behaviour changes at this stage.

One subtlety in `hostvis_map_blob`: its "the handle vanished, undo the map" rollback
(`:3575-3590`) must now key on the **object**, not the handle. A concurrent `GEM_CLOSE`
of one handle no longer means the object is gone — another open may still hold it. The
correct test is "the object still exists", and the rollback fires only when it does not.
Getting this wrong resurrects the window-span leak the current code was written to
avoid, in the one case where two opens race a first map.

### 3.2 The dmabuf-fd reference (§2.4, §2.5)

`TmpVmo.dmabuf_obj`, the `vmo_free_slot` return value, the three call-site changes, and
the boot-time hook registration. Independently due as a bug fix.

### 3.3 `PRIME_FD_TO_HANDLE` mints an importer handle

`kernel/src/syscall.rs:6110`. Today:

```rust
let handle = match vfs::dmabuf_handle_of(sched::tgid_of(pid), dfd as usize) { ... };
unsafe { (arg as *mut u32).write(handle); }
```

Becomes: resolve `(dmabuf_obj, dmabuf_handle)` from the fd; if `dmabuf_obj != 0`, call
`drivers::drm_device_interface::blob_import(obj, open_id)`, which returns an existing
handle for that `(obj, open_id)` or mints a new one and takes a reference; write that
back. The dumb path is unchanged.

**`open_id` is the problem here, and it is real.** The PRIME arm is intercepted in
`sys_ioctl` *before* the DRM server is reached, and `sys_ioctl` has `fd`, not `open_id`.
The DRM server receives `open_id` in ioctl slot 4 (`servers/drm/src/lib.rs:159`) because
the VFS puts it there; the syscall layer never looks it up. It can:
`vfs::vfs_get_node_kind(pid, fd)` already returns
`VnodeKind::DynamicDevice { port, dev_id, open_id }` and is already called from
`kernel/src/syscall.rs:1602` for exactly this reason (the mmap proxy). Reuse that, and
refuse with `-EINVAL` if the fd is not a DynamicDevice — which is also the correct
answer for `PRIME_FD_TO_HANDLE` on a non-DRM fd, something the current code does not
check at all.

Note that `PRIME_HANDLE_TO_FD` (the export side) needs the same `open_id` and for the
same reason: exporting a blob must go through `blob_lookup(handle, open_id)` so one open
cannot export another's buffer to itself. The landed patch resolves blobs through
`blob_lookup` but is handed whatever open the syscall layer can find; confirm this when
implementing rather than assuming.

### 3.4 `MODE_MAP_DUMB` accepts a blob handle

`std_handle_map_dumb` (`:1941`) currently consults `DUMB_BUFFERS` only. It gains a blob
branch that returns the same token `VIRTGPU_MAP` would: `map_phys` for a HOST3D blob
(performing the first `hostvis_map_blob` if `map_phys == 0`), `phys` for a guest-backed
one. This is what makes kms_swrast's importer work — it does `PRIME_FD_TO_HANDLE`,
`lseek(SEEK_END)`, `MODE_MAP_DUMB`, `mmap`, and never issues a virtgpu ioctl.

Three implementation constraints, all load-bearing:

* **It must take `open_id`.** Add it to the signature and to the dispatch arm at
  `:1416`. Without it the blob branch would be unscoped and would re-open the hole
  §2.2 closes — the compositor could map any blob by number.
* **It must stop dereferencing user memory under the device lock.** The arm at `:1416`
  takes `get_drm_device().lock()` and the body does
  `&mut *(arg as *mut drm_mode_map_dumb)` (`:1943`) — a write to user memory under a
  lock, which is the `82d0cc3` class the project bans. The current body cannot fault
  in practice only because the caller just wrote the struct; the blob branch makes the
  window much wider (it can now take `VIRTIO_GPU` and busy-spin on a device round-trip
  in between). Restructure as every virtgpu handler already does: read `handle` before
  any lock, drop every lock, write `offset` last. The device lock is not needed by this
  handler at all — `_device` is already unused.
* **The first map may now be performed by the importer.** That is correct:
  `RESOURCE_MAP_BLOB` is per-resource, and the resulting token is a guest-physical
  range that both processes may map independently. It does mean `hostvis_alloc`
  pressure is now driven by importers too; `MAX_HOSTVIS_SPANS = 256` (`:891`) is the
  cap and a compositor holding one span per client swapchain image will approach it
  (3 images x N clients). Raise it or measure it; do not discover it as a hang.

### 3.5 Not needed for M4, though item 8 lists it

`CTX_ATTACH_RESOURCE` **for the importer's context**. cosmic-comp's Mesa is
kms_swrast/softpipe: it never issues `VIRTGPU_CONTEXT_INIT`, so `ctx_lookup(open_id)`
returns 0 and the attach is a no-op.

It becomes necessary the day the *importer* is itself a 3D client — concretely, Venus
importing another guest process's buffer, which is a real Mesa path:
`virtgpu_bo_create_from_dma_buf` (`mesa/src/virtio/vulkan/vn_renderer_virtgpu.c:1152`)
does `PRIME_FD_TO_HANDLE` (`:1165`), then `RESOURCE_INFO` (`:1170`), then **refuses the
import unless `info.blob_mem` equals its own `bo_blob_mem`** (`:1181-1184`), and
`GEM_CLOSE`s the handle on failure (`:1240-1241`). Reached from
`vn_device_memory_import_dma_buf` and `vn_GetMemoryFdPropertiesKHR`
(`vn_device_memory.c:110-124`, `:564-575`).

The design accommodates that at zero cost — `ctx` already moves onto `BlobHandle` in
§2.2, so import attaches the importer's context and handle-destroy detaches that one and
only that one — but it should not be implemented or tested until something needs it. Note
that Stage 3's assertion 2 (`RESOURCE_INFO` reporting the *original* `blob_mem` through
an imported handle) is precisely what that Mesa check requires, which is why it is in the
test rather than being an obvious invariant.

---

## 4. Explicitly deferred: the `VK_KHR_display` half

Not in the M4 path, and cited here so the deferral is a decision rather than an
omission:

* **`ADDFB2` accepting blob handles** (`:2679` resolves only `DUMB_BUFFERS`). Needed to
  scan out a Vulkan-rendered buffer directly. M4 presents through the compositor, which
  composites into its own dumb scanout buffer.
* **`SET_SCANOUT_BLOB`** — does not exist in `drivers/src/virtio_gpu.rs` at all. This is
  the real work in the display half: a new virtio-gpu command plus the modeset path to
  drive it.
* **The connector's missing `DPMS` property** — `VK_KHR_display` enumerates and powers
  display planes; `PROPS` (`:164`) has no `DPMS`.
* **`VK_KHR_display` itself** is not a milestone this project has committed to. It
  displaces the compositor rather than integrating with it, and on a headless QEMU guest
  its only observable is a screendump — which `--present` already produces via the dumb
  path (item 4).

Recommendation: keep all four out of item 8. If they are wanted, they are a separate
item with a separate justification.

---

## 5. Staging

Six stages, numbered so the two that are due regardless of M4 come first. Stage 0 is a
measurement pass whose job is to kill Stages 3–5 cheaply if they cannot work.

### Stage 0 — two measurements, before any code (1–2 h, no kernel change)

Both are go/no-go for Stages 3–5. Neither requires a build.

**0a. Does cosmic-comp advertise `zwp_linux_dmabuf_v1` in our configuration?**

This is the hard gate, and it is *upstream policy*, not a kernel gap — see §6 for why it
probably decides the whole item. The measurement: extend `leandros-applet` (the existing
dependency-free `wayland-client` binary already staged at `/bin/leandros-applet`) to
print every `wl_registry.global` it receives and exit. Run it inside a live COSMIC
session on aarch64/HVF. The single line that decides everything is whether
`zwp_linux_dmabuf_v1` appears; whether `wl_drm` appears alongside it is the corroborating
read, because the two globals are created three lines apart
(`cosmic-comp/src/backend/kms/socket.rs:55-59`) behind the same `!is_software` gate.

Cost: under an hour, one existing crate, one existing session harness, no kernel rebuild.
This must be the first thing anyone does on this item.

**0b. Is the WSI swapchain image blob CPU-readable?**

The compositor's import ends in Mesa's `kms_swrast` CPU mapping
(`PRIME_FD_TO_HANDLE` → `lseek(SEEK_END)` → `MODE_MAP_DUMB` → `mmap`,
`src/gallium/winsys/sw/kms-dri/kms_dri_sw_winsys.c:382,401,325,332`), so the exported
blob must be mappable. Direct scanout cannot avoid it: smithay refuses
`DRM_FORMAT_MOD_INVALID` buffers for scanout (`smithay efeb597
src/backend/drm/gbm.rs:87`) and INVALID is the only modifier an EGL stack without
`EGL_EXT_image_dma_buf_import_modifiers` can produce — smithay hardcodes exactly
`{ARGB8888, XRGB8888} × Modifier::Invalid` in that case
(`src/backend/egl/display.rs:895-896`, `:993-1001`).

Mappability is decided by Venus at allocation and is **kernel-observable at zero cost**.
`virtgpu_bo_blob_flags` (`mesa/src/virtio/vulkan/vn_renderer_virtgpu.c:1134-1148`) sets
`VIRTGPU_BLOB_FLAG_USE_MAPPABLE` **iff the Vulkan memory type has
`VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT`** (`:1139-1140`). A swapchain image's memory is
selected by `wsi_select_device_memory_type`, which prefers device-local. So the expected
answer is *not mappable*, and if so the softpipe compositor has no way to read those
pixels at all.

*The measurement:* add a `serial_debug` line in `virtgpu_handle_resource_create_blob`
(`drivers/src/drm_device_interface.rs:3330`) printing `blob_mem`, `blob_flags` and
`size` for every create, and run the **already-built** `vkswap` — its swapchain images go
through the same `wsi_create_native_image_mem` a Wayland swapchain would.

*Decision rule.* `USE_MAPPABLE` set on the image-sized blobs → Stage 4 is possible.
Not set → Route A is dead as designed, and the only remaining variant is Mesa's WSI
prime-blit path, discussed and dismissed in §6.3.

### Stage 1 — object/handle split, refcounted, no behaviour change (≈1 day)

**Land this whatever is decided about M4.** Deliverable: §2.2 and §3.1, plus a
`BLOB_OBJS_LIVE` counter on the `[DRMSTAT]` line and a
`VIRTGPU_PARAM_LEANDROS_BLOB_OBJS` getparam so userspace can read it.

*Gate.* `venustest` 77/0, `vkrender` `s2_checksum = 0x02C0FDC5`, `vktest` 0 failures,
`drmsmoke` 22/0, `scmtest` 30/0 — all unchanged, both arches, fresh images, `vfstest`
run exactly once.

*Guard test — `blob_obj_freed_on_last_handle`:* create a blob, read `BLOB_OBJS` (== 1),
`GEM_CLOSE`, read it again (== 0).
*Mutation that must make it fail:* make `blob_unref` decrement but never take the zero
arm. The counter then reads 1 after close → FAIL. Apply the mutation and watch it fail
before trusting the test — `memfd_inflight_close` (`77f170d`) is the precedent for a
guard that passed with its guard removed.

### Stage 2 — the dmabuf fd holds a reference (≈half a day)

**Land this whatever is decided about M4** — it is the §2.4 use-after-free, and it is
live today. Deliverable: §2.4 and §2.5, for **both** blobs and dumb buffers.

*Guard test 1 — `export_fd_keeps_blob_alive`:* create a guest-backed blob, write a known
pattern through `VIRTGPU_MAP` + `mmap`, `PRIME_HANDLE_TO_FD`, `GEM_CLOSE`, then
`read(fd)` and assert the pattern is intact and `BLOB_OBJS == 1`; `close(fd)` and assert
`BLOB_OBJS == 0`.
*Mutation:* delete the `blob_ref` in `install_dmabuf_vmo`. The `read` then returns
recycled memory and `BLOB_OBJS` reads 0 after `GEM_CLOSE` → FAIL on both assertions.
This test **fails at `HEAD` + `e083202`** by construction — it is a regression test for a
live bug, which is the strongest form of the project's guard-test rule.

*Guard test 2 — no double release:* after `close(fd)` brings the count to 0, assert the
serial log has no `[DRM] blob refcount underflow` line and that a subsequent blob create
succeeds. A double `mm::buddy::free` of an order-N block is allocator corruption, and the
next `alloc` is the cheapest detector available.

*Compositor regression gate — genuinely new risk.* Keeping a **dumb** buffer alive until
its export fd closes changes cosmic-comp's steady state, because it exports a dmabuf per
frame. Run a 60 s aarch64 session with `DRM_STATS` on and assert `DUMB_LIVE` and
`BLOB_OBJS_LIVE` are *bounded*, not monotonically climbing. If they climb, Mesa is
holding fds whose buffers we were previously freeing underneath it — which is information
worth having, not a reason to revert the fix.

### Stage 3 — cross-open import (≈half a day + test) — **only if 0a says go**

Deliverable: §3.3, plus the `DESTROY_DUMB` correction from §1.2. New test binary
`dmabuftest` (or a `venustest` phase 6).

*Guard test — `import_mints_a_distinct_handle`:* open card0 twice in one process
(two `open_id`s, per `servers/drm/src/lib.rs:55-58`); create a blob on A, export, import
on B. Assert:

1. `handleB != handleA` — *mutation: return the exporter's handle → FAIL.*
2. `RESOURCE_INFO(fdB, handleB)` reports A's `res_handle`, `blob_mem` and page-aligned
   size — *mutation: mint a fresh object instead of referencing → `res_handle` differs →
   FAIL.* (This assertion is not academic: Venus's own importer refuses a bo whose
   `info.blob_mem` does not equal its `bo_blob_mem`,
   `mesa/src/virtio/vulkan/vn_renderer_virtgpu.c:1181-1184`.)
3. `RESOURCE_INFO(fdB, handleA)` is refused with `ENOENT`. **This is the security
   assertion and the one that must survive every later change.** *Mutation: make
   `open_may_reach` return `true` → FAIL.*
4. importing the same fd on B a second time returns `handleB` and leaves `BLOB_OBJS` at 1
   — *mutation: drop the idempotence lookup → a second handle appears, the count reaches
   3 → FAIL.*
5. `MODE_DESTROY_DUMB(fdB, handleB)` leaves `BLOB_OBJS == 1`; only closing A's handle and
   the fd reaches 0 — *mutation: leave `DESTROY_DUMB` unchanged → B's handle leaks, the
   count never reaches 0 → FAIL.*

*Cross-process subtest — `crossproc_import`:* `socketpair`, `fork`, child opens card0
itself, parent sends the dmabuf fd with `SCM_RIGHTS` (`scmtest` already has the
machinery), child imports and reads the pattern the parent wrote. This is the M4 shape
minus Wayland, and it is the last stage that can be verified without a compositor.

### Stage 4 — `MODE_MAP_DUMB` accepts a blob handle (≈half a day) — **only if 0b says go**

Deliverable: §3.4.

*Guard test — `crossopen_map_is_coherent`:* parent writes `0xA5` through its
`VIRTGPU_MAP` mapping; child imports, `MODE_MAP_DUMB`, `mmap`, reads `0xA5`; child writes
`0x5A` and the parent sees it. *Mutation: return `b.phys` instead of `b.map_phys` for a
HOST3D blob → the child maps nothing or zeroes → FAIL.* Second mutation: drop the
`open_id` argument and skip `blob_lookup` → the negative subtest ("map a handle this open
never imported") FAILs.

*Also assert:* `HOSTVIS_SPANS.len()` on the `[DRMSTAT]` line against `MAX_HOSTVIS_SPANS`
(256, `drivers/src/drm_device_interface.rs:891`) with three swapchain images live per
client. Discover the ceiling with a counter, not with a hang.

### Stage 5 — the client (1–2 days, userspace only)

`vkwl`: `VK_KHR_wayland_surface` + swapchain + the `vkrender` subtest-2 triangle. Build
recipe is `vkrender`'s (`-std=gnu11`, private `vulkan` + `vk_video` include dir, dynamic
link; the zig-cc static-binary and UBSan traps are recorded in
`~/code/leandros-artifacts/notes/m9-m3-vulkan/`).

*Gate.* A screenshot of the COSMIC session showing the triangle, both arches, plus
kernel-side counters: `PRIME_IMPORTS >= 3` (one per swapchain image) and `BLOB_OBJS_LIVE`
flat across 600 presents. The counter gate matters more than the screenshot — a leak of
one object per present is exactly what this design exists to prevent and is invisible in
a picture.

---

## 6. Whether to do this at all — the alternatives, and the finding that decides it

### 6.1 cosmic-comp does not offer dmabuf to clients on a software renderer

The dmabuf global is created only for a KMS device that is not software:

```rust
// cosmic-comp/src/backend/kms/device.rs:760-761
let socket = match (!is_software)
    .then(|| common.create_socket(dh, render_node, texture_formats.clone()))
```

`is_software` is `egl.device.is_software()` (`device.rs:718,732`), which is precisely
"the EGL device advertises `EGL_MESA_device_software`" (smithay `efeb597`,
`src/backend/egl/device.rs:241-245`). Both `zwp_linux_dmabuf_v1` and `wl_drm` live behind
that gate (`kms/socket.rs:55-59`), and the global is additionally filtered per client on
`advertised_node_for_client(client) == Some(render_node)` (`socket.rs:47`,
`cosmic-comp/src/state.rs:819-822`), where the node comes from `determine_primary_gpu` —
which itself refuses software devices (`kms/mod.rs:270-273`) and is the reason this
project must set `COSMIC_RENDER_DEVICE=226:0` at all.

**There is no build-configuration escape.** `cosmic-comp/Cargo.toml:119-125` has no
feature gating dmabuf; the switch is purely runtime and purely
`EGL_MESA_device_software`. Turning the global on for a software device would require
patching cosmic-comp, which the project forbids. If Stage 0a finds the global absent,
**item 8 cannot unblock a Wayland Vulkan client at any amount of kernel effort**, and
that is a feasibility finding rather than a scheduling one.

The counterpart failure is hard, not graceful: a non-`sw` Vulkan driver on a compositor
without `zwp_linux_dmabuf_v1` returns `VK_ERROR_SURFACE_LOST_KHR`
(`mesa/src/vulkan/wsi/wsi_common_wayland.c:1544-1549`), because the registry handler
binds `wl_shm` *only* in the `sw` case and `zwp_linux_dmabuf_v1` *only* in the non-`sw`
case (`:1406-1421`) — they are mutually exclusive.

### 6.2 The route that needs no kernel work at all: `MESA_VK_WSI_DEBUG=sw`

Mesa decides dmabuf-vs-shm in one place, on one boolean:

```c
/* mesa/src/vulkan/wsi/wsi_common.c:87 */
wsi->sw = device_options->sw_device || (WSI_DEBUG & WSI_DEBUG_SW);
```

`WSI_DEBUG` is parsed from the **environment variable `MESA_VK_WSI_DEBUG`**
(`wsi_common.c:78`, flag table `:52-59`, `sw` bit in `wsi_common_private.h:38-43`) in
ordinary release builds. Setting `MESA_VK_WSI_DEBUG=sw` puts Venus's Wayland WSI on
`WSI_IMAGE_TYPE_CPU` (`wsi_common_wayland.c:3546-3556`), where images are allocated as
linear host-visible Vulkan images and presented through a `wl_shm` pool backed by
`os_create_anonymous_file` + `mmap` (`:3215-3235`, pool at `:3261`).

Every piece of that already works on LeandrOS:

* memfd + `MAP_SHARED` is the panel/applet path and was hardened in `77f170d`
  (`memfd_anonymous_reclaim` 300/300, `memfd_inflight_close`);
* `wl_shm` is what every COSMIC client already uses;
* host-visible Vulkan memory mapped and read by the CPU is exactly what `vkrender`'s
  pinned `s2_checksum = 0x02C0FDC5` proves works on both arches.

This is **not** a hand-rolled blit dressed up as WSI. It is `vkCreateWaylandSurfaceKHR`,
`vkCreateSwapchainKHR`, `vkAcquireNextImageKHR` and `vkQueuePresentKHR` through Mesa's
own shipped Wayland WSI, taking the same branch Mesa takes on any system whose renderer
lacks dmabuf. It is an environment variable, which this project's rules permit
explicitly, and it costs **zero kernel days**.

Note that Venus reaches the same branch on its own when the *host* driver lacks
`VK_EXT_external_memory_dma_buf` (`mesa/src/virtio/vulkan/vn_wsi.c:69-82` sets
`sw_device` itself), so this is a supported configuration, not a debug hack that happens
to work.

Residual risk to check when doing it: with `has_import_memory_host` false, the buffer
type is `WSI_WL_BUFFER_SHM_MEMCPY` (`wsi_common_wayland.c:3555`), i.e. one full-image
`memcpy` per present. At 1280×800 that is 4 MB per frame — slow, and the right thing to
measure rather than assume, but it is a performance number, not a blocker.

### 6.3 Why the prime-blit variant does not rescue the dmabuf route

If Stage 0b finds the swapchain blob unmappable, the obvious next idea is Mesa's WSI
prime-blit path, which allocates a separate linear presentation buffer and blits into it.
Venus already takes it: `supports_scanout = false` (`vn_wsi.c:86`) plus
`num_modifier_lists == 0` satisfies `wsi_drm_image_needs_buffer_blit`
(`wsi_common_drm.c:893-901`). But `wsi_configure_prime_image` selects that buffer's
memory with `wsi_select_device_memory_type` when `same_gpu` holds
(`wsi_common_drm.c:867-870`) — device-local again, hence `USE_MAPPABLE` again unset — and
`same_gpu` stays `true` for Venus because it passes `display_fd = -1`
(`vn_wsi.c:78`), leaving `drm_info.hasRender`/`hasPrimary` false so the
feedback-based override at `wsi_common_wayland.c:1560-1585` never fires. There is no
environment variable that forces the DRM prime-blit path or its memory-type choice
(`MESA_VK_WSI_DEBUG=buffer` affects only the CPU/shm path,
`wsi_common.c:2479-2480`). So the variant does not change the answer without patching
Mesa.

### 6.4 What item 8 is actually worth, once M4 is off it

Stripped of M4, cross-open dmabuf import buys:

* a **zero-copy** client→compositor path instead of §6.2's per-frame `memcpy` — real, but
  worth nothing until the compositor stops being softpipe, since a software compositor
  reads every pixel with the CPU regardless;
* `VK_KHR_display`, which needs the whole §4 list on top and is not a committed
  milestone;
* Venus **importing** a foreign dmabuf (`vn_device_memory.c:110-124`,
  `vn_get_memory_dma_buf_properties`), i.e. Vulkan-to-Vulkan buffer sharing between two
  guest processes. This is the one consumer whose value does not depend on the compositor
  being accelerated, and it is fully served by Stages 1–3 without Stage 4.

---

## 7. Estimate

| Stage | Work | Cost | Depends on |
|---|---|---|---|
| 0a | applet global dump, one COSMIC run | 1 h | — |
| 0b | `serial_debug` in blob create, re-run `vkswap` | 1 h | — |
| 1 | object/handle split + refcount + counters + guard test | 1 day | — |
| 2 | dmabuf-fd reference, vfs hook, dumb parity, 3 gates | 0.5 day | 1 |
| 3 | import mints a handle, `DESTROY_DUMB` fix, `dmabuftest` | 0.5–1 day | 1, 2, 0a |
| 4 | `MAP_DUMB` blob branch + lock-discipline repair | 0.5 day | 3, 0b |
| 5 | `vkwl` client, both arches, screenshot + counter gates | 1–2 days | 4 |

Both-arch QEMU regression per stage is inside those numbers; the toolchain cost for
Stage 5 is not (it is the `vkrender` recipe, already paid once).

* Stages 1+2 alone: **1.5 days**, and they are a bug fix.
* Full route A, if 0a and 0b both say go: **4–6 days**, matching item 8's "several days"
  — but now with a kill switch that costs two hours instead of four days.
* Route §6.2: **1–2 days**, all userspace, zero kernel risk.

Confidence: high on Stages 1–3 (the code is small, bounded, and the shape is upstream's).
Low on Stages 4–5, because both rest on Mesa and cosmic-comp behaviour that has not been
measured on target.

---

## 8. Recommendation

**Do not put M4 behind item 8.**

1. **Run Stage 0a today** (one hour). If `zwp_linux_dmabuf_v1` is absent from a live
   COSMIC session, Stages 3–5 are dead upstream and no kernel work revives them without
   patching cosmic-comp. Record the result either way — it is a fact this project will
   need again.
2. **Take M4 via `MESA_VK_WSI_DEBUG=sw`** (§6.2). It is real Vulkan WSI, real
   `vkQueuePresentKHR`, an environment variable rather than a source patch, and zero
   kernel days. It is also the correct bisection point for the dmabuf route: it proves
   the client, the protocol, the compositor wiring and the Vulkan rendering
   independently, so if the dmabuf route is later attempted, the only new variable is the
   kernel.
3. **Land Stages 1 and 2 anyway**, on their own merits, and retitle the TODO item to say
   so. As written, item 8 is filed as a *Feature* blocked on nothing. Half of it is a
   **use-after-free reachable from one unprivileged process** (§2.4) that the PRIME
   export just widened from dumb buffers to blobs, and it deserves to be an item-1-class
   bug with its own regression test, not a line inside a deferred feature.
4. **Defer Stages 3–5, and §4's display half, explicitly.** Revisit Stage 3 when
   Vulkan-to-Vulkan sharing between two guest processes is wanted (§6.4), and Stage 4
   only if the compositor ever stops being softpipe.

The one thing not to do is start at Stage 1 with M4 as the justification. The
justification for Stages 1–2 is a live memory-safety bug; the justification for Stages
3–5 has not been established and can be checked for the price of a single COSMIC session.

---

## 9. Open questions this design does not settle

1. **Is `egl.device.is_software()` true for our card0 EGL device?** Stage 0a answers it
   empirically. Source reading cannot: Mesa flags the pure-swrast EGL device with
   `EGL_MESA_device_software`, but a gbm/`kms_swrast` display on a real DRM fd is a
   different device object, and this project's own note that `COSMIC_RENDER_DEVICE` is
   needed because `determine_primary_gpu` "filters our software device out" cuts both
   ways.
2. **Does `mm::buddy::free` of an order-N block honour per-frame `mm::pageref` counts?**
   It does not (`mm/src/pageref.rs:39-51` is the only pageref-aware free), so a process
   that `mmap`s its own dmabuf, closes the fd and drops the last handle keeps a live
   `MAP_SHARED` mapping of freed frames. Pre-existing on the dumb path, unchanged by this
   design, and it deserves its own item rather than being smuggled into this one.
3. **`handle_ioctl_mmap` validates the mmap token globally, not per open**
   (`drivers/src/drm_device_interface.rs:2793-2799` scans every `DUMB_BUFFERS` and
   `BLOB_BUFFERS` value). Guessing a guest-physical token is not trivial, but the
   host-visible window base is deterministic. The fix is cheap — the mmap proxy already
   carries `open_id` (`kernel/src/syscall.rs:1602`) — and it becomes more valuable once
   importers can map, so it belongs with Stage 4 if Stage 4 happens.
4. **Does the landed `PRIME_HANDLE_TO_FD` receive a truthful `open_id`?** §3.3 notes the
   syscall layer has `fd`, not `open_id`. Confirm what the export path is actually
   passing before building on it; if it passes 0, `open_may_reach`'s `caller == 0` arm
   means *any* open can currently export *any* blob, which would be a second, smaller
   hole in the same area.
