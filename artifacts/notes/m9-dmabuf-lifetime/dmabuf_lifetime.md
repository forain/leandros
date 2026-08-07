# dmabuf lifetime — an exported fd now keeps its buffer alive (Stages 1 + 2)

Lane Q, 2026-08-06. Worktree only; **no QEMU was run** (both machines were owned by other
lanes). Every claim below is a source citation, a build result, an applied-patch result,
or an explicitly labelled falsifiability argument.

Scope is Stages 1 and 2 of `notes/m9-crossopen-dmabuf/crossopen_design.md`. Stages 3–5
are **not** implemented. `open_may_reach` is **byte-identical** to what the PRIME export
left it as; the only edit anywhere near it is one comment that said
`prime_export_backing` and now says `prime_export_acquire`.

---

## 0. Base, and how to apply this

* Worktree base commit: **`a0f2c46`** (the Mac's `main` tip).
* **`notes/m9-prime-export/prime_handle_to_fd_built_20260806.patch` applied first**
  (`git apply --check` clean). That is the landed PRIME export `e083202`, not yet synced
  to the Mac.
* `dmabuf_lifetime.patch` is a diff against **`a0f2c46` + that patch**. It does not apply
  to bare `a0f2c46` and is not meant to. Apply order: PRIME, then this.

Verified by round-trip: the emitted patch was re-applied to a clean `a0f2c46 + PRIME`
tree and reproduces the working tree exactly (`git apply --check --reverse` clean).

```
 drivers/src/drm_device_interface.rs | 679 ++++++++++++++++++++++++++++--------
 kernel/src/syscall.rs               |  24 +-
 servers/drm/src/lib.rs              |  12 +
 servers/vfs/src/lib.rs              | 202 +++++++++--
 userland/venustest/src/main.rs      | 397 +++++++++++++++++++++
 5 files changed, 1131 insertions(+), 183 deletions(-)
```

---

## 1. The defect, restated in one paragraph

`release_blob` and `free_dumb` called `mm::buddy::free(phys, order)` the instant the gem
handle went away. `vmo_free_slot` (`servers/vfs`) returns early for a borrowed VMO
*without* freeing, on the stated grounds that the DRM layer frees the block exactly once.
Nothing anywhere made the DRM object outlive an exported dmabuf fd. So from **one
unprivileged process, with no cross-open work at all**, `CREATE_BLOB → PRIME_HANDLE_TO_FD
→ GEM_CLOSE → read(fd)` walked freed frames through the HHDM and returned whatever the
buddy allocator had since handed them to, and `mmap(fd, MAP_SHARED)` was the same hazard
with writes. Pre-existing on the dumb path; widened to blobs by the PRIME export.

---

## 2. The refcount model as implemented

### 2.1 Objects and handles

```rust
static NEXT_BO_OBJ: AtomicU32 = AtomicU32::new(1);   // ONE id space, both BO kinds

struct BlobObj {   // BLOB_OBJS: BTreeMap<u32 /*obj*/, BlobObj>
    phys, order, res_handle, size, blob_mem,
    last_fence,                  // on the OBJECT: a submission fences the buffer
    win_off, map_phys, map_info, // per RESOURCE, not per open
    refs: u32,
}

struct BlobHandle {  // BLOB_BUFFERS: BTreeMap<u32 /*gem handle*/, BlobHandle>
    obj: u32,
    owner: u32,   // unchanged semantics — open_may_reach is untouched
    ctx: u32,     // MOVED here: attachment is per-open
}
```

`BLOB_BUFFERS` keeps its key space (`NEXT_BLOB_HANDLE`, from `0x4000`) and its value
becomes `BlobHandle`, which **is one reference**. `blob_lookup` returns a `BlobView` —
the handle joined to its object — so every one of the ~10 consumers reads one flat record
and did not have to change.

Dumb buffers get the *same* treatment without a second map, because they have no import
path and so never need two handles per object:

```rust
struct DumbBuf {                 // DUMB_BUFFERS unchanged as a map
    phys, order, last_fence,
    obj: u32,                    // same NEXT_BO_OBJ space
    refs: u32,
    handle_live: bool,           // false once DESTROY_DUMB/GEM_CLOSE retired the handle
}
```

`handle_live` is what keeps handle *resolution* byte-identical to before: a retired
record survives only to keep an exported fd's frames alive, and `dumb_lookup` — now the
single funnel for `MAP_DUMB`, `ADDFB`, `ADDFB2`, `bo_exists`, `bo_fence`,
`dumb_buffer_phys_order`, `virtgpu_handle_map`'s dumb arm and `handle_ioctl_mmap`'s token
scan — filters it out. The handle number is exactly as dead as it was before.

**Why one shared id space.** It makes the VFS→drivers hook a single `fn(u32)`, and it
makes "the fd remembers an object id, never a gem handle" literally true for **both** BO
kinds. `bo_release_exported(obj)` tries `BLOB_OBJS` then scans `DUMB_BUFFERS` for a
matching `obj` (a map with a handful of entries, consulted only on dmabuf-fd close).

### 2.2 Where each reference is taken and dropped

| # | Reference | Taken | Dropped |
|---|---|---|---|
| 1 | the blob gem handle | `virtgpu_handle_resource_create_blob` inserts `BlobObj{refs:1}` then the `BlobHandle` | `free_blob_owned` (GEM_CLOSE) / `free_blob` (`drm_release_open`) → `blob_unref(h.obj, h.ctx)` |
| 2 | the dumb gem handle | `DrmDumbBuffer::create` inserts `refs:1, handle_live:true` | `free_dumb` (DESTROY_DUMB and GEM_CLOSE) — clears `handle_live` and decrements, **idempotent** |
| 3 | the exporting `TmpVmo` slot | `prime_export_acquire`, under the object map, atomically with the resolution | `vmo_free_slot` returns the id → caller calls `dmabuf_release` → `bo_release_exported` |

Reference 3 is **per slot, not per fd**. `TMP_VMOS` is keyed by the data-owning slot, so
dup / fork / SCM_RIGHTS copies of one dmabuf fd already share one slot, and that slot is
destroyed exactly once, by `vmo_free_slot`. One ref per slot is both sufficient and
impossible to double-drop.

Ownership of reference 3 is explicit in the syscall layer, which now has three
dispositions and no fourth:

* `prime_export_acquire` returns `Some` → the syscall owns one reference;
* tmpfs open fails → `bo_release_exported(backing.obj)`, then return;
* `install_dmabuf_vmo` returns `false` → close/unlink the ephemeral node, then
  `bo_release_exported(backing.obj)`;
* `install_dmabuf_vmo` returns `true` → the slot owns it.

`install_dmabuf_vmo` also now **refuses** to overwrite a live VMO slot. That cannot fire
today (the node was created one instruction earlier), but if it ever did, the displaced
slot's reference would be dropped on the floor and its buffer pinned for the boot.

### 2.3 Double-release guards

The opposite failure — the `9be954f` class — is guarded structurally, not by convention:

* every decrement is a **test-and-remove under one acquisition** of the object map
  (`blob_unref`, `dumb_unref_by_obj`, `free_dumb`), so two racing droppers cannot both
  observe zero;
* there is **no `release_blob(record)` entry point any more**. The teardown body lives
  inside the zero arm, operating on the record that arm removed, so there is nothing a
  caller that "knows" the count could call. This is a deliberate tightening of the design
  doc's wording, which still had a separate `release_blob`.
* an unref of an id in neither registry logs `[DRM] bo refcount underflow obj=<id>` and
  frees nothing.

### 2.4 A latent lock bug fixed on the way past

`free_blob` was

```rust
if let Some(b) = BLOB_BUFFERS.lock().remove(&handle) { Self::release_blob(b); }
```

A temporary guard in an `if let` scrutinee lives to the end of the whole `if let`, so the
entire teardown — including `VIRTIO_GPU.lock()` and a busy-spun device round-trip — ran
with `BLOB_BUFFERS` held. `free_blob_owned` was already written as a `let taken = { … };`
block for exactly that reason. `free_blob` now matches it. This was reachable from
`drm_release_open`, i.e. every card0 close.

---

## 3. The lock-ordering argument for the release hook

**The constraint.** `vmo_free_slot` runs from tmpfs inode teardown with `TMP_FILES` held
(its own doc comment says so). The release reaches `blob_unref`, which takes `VIRTIO_GPU`
and busy-spins on a device round-trip. Calling straight through would (a) hold a tmpfs
lock across a device round-trip and (b) add a second lock order, `TMP_FILES → VIRTIO_GPU`,
to a codebase that has one. This project froze all four vCPUs once on exactly that shape
(`82d0cc3`).

**The shape as built.**

1. `vmo_free_slot(owner) -> Option<u32>` takes no new lock and calls nothing. It is
   `#[must_use]`, so a caller that forgets is a compile error rather than a silent leak.
2. `tmp_drop_name(...) -> Option<u32>` likewise — it runs with the *caller's* guard alive
   and so cannot release either. Also `#[must_use]`.
3. Each of the four sites captures the value, lets its guards die, then calls
   `dmabuf_release`:
   * `tmp_release_ephemeral` — `drop(tmp); drop(tbls); dmabuf_release(..)` at the bottom.
     **This is the site that matters**: the PRIME intercept unlinks `/tmp/dmabuf:<n>`
     immediately, so the node is nameless and this is the only path that collects a
     dmabuf slot.
   * `handle_unlink` — restructured to `let (reply, freed_obj) = match …;` then
     `drop(tmp); dmabuf_release(..); return reply;`
   * `tmpfs_rename` — a `clobbered_obj` local carried to all three exits after the
     `tmp_drop_name` call. The ENAMETOOLONG early return inside a `for … in tmp.iter()`
     could not drop the guard while an iterator borrowed it, so that loop is now an
     `any(..)` with the return outside it.
4. The hook itself is `static DMABUF_RELEASE: AtomicUsize` + `pub fn set_dmabuf_release`,
   because `vfs-server` depends on `ipc, sched, mm, xattr, spin` and **must not** gain
   `drivers`.

**Registration point: `servers/drm/src/lib.rs::init`, not the kernel.** That crate already
depends on both `drivers` and `vfs_server`, so no new dependency edge is created anywhere.
A build with no DRM device never registers, and the null check makes that a no-op — which
is correct, because nothing can have exported.

**The reverse order cannot arise**: `bo_release_exported` takes no VFS lock and touches no
user memory. And `release_vnode`'s documented contract is already "caller must NOT hold
the FD_TABLES lock", which `handle_close` honours by dropping `tbls` before the match.

Within the driver, `BLOB_BUFFERS` (handles) and `BLOB_OBJS` (objects) are separate leaf
locks taken **one at a time, never nested** — the same property `BLOB_BUFFERS` /
`DUMB_BUFFERS` already had. `blob_lookup` reads the handle map, drops it, then reads the
object map; a concurrent close between the two makes the object lookup miss, reported as
`None`, which is the correct "this handle names nothing". `VIRTIO_GPU` is taken with no BO
map held, ever. `blob_unref` additionally **skips the device lock entirely** when there is
nothing to say to the device (an fd reference going away while others remain) — that is
the compositor's per-frame dmabuf-close path.

---

## 4. The regression test, and its falsifiability

`userland/venustest` gains **phase 6**, `--- phase 6: exported dmabuf keeps the buffer
alive ---`. Two halves: blobs, and the pre-existing dumb path.

### 4.1 The sequence

Exactly the one from the brief, with a pattern and a churn loop:

1. `RESOURCE_CREATE_BLOB(GUEST, 0x3000)` — deliberately not a power of two.
2. `VIRTGPU_MAP` + `mmap`, fill all 0x3000 bytes with `pat_byte(i) = ((i*7) ^ 0x5A) & 0xFF`.
3. `PRIME_HANDLE_TO_FD`.
4. `GEM_CLOSE`.
5. **churn**: create 8 more same-size guest blobs and keep them alive.
6. `read(fd)` the whole 0x3000 and require every byte back.
7. `mmap(fd, MAP_SHARED)` and require the same.
8. `munmap`, close the churn handles, `close(fd)`, require the object count back to
   baseline, and require a fresh blob create to still succeed.

The dumb half is the same with `CREATE_DUMB` / `MAP_DUMB` / `DESTROY_DUMB`.

### 4.2 Why the churn loop is load-bearing, not padding

**This is the part that would otherwise make the test pass against the bug it exists to
catch.** `mm::buddy::free` does not scrub, so a `read()` straight after `GEM_CLOSE` very
often returns the pattern anyway. The churn forces the frames back into circulation, and
`virtgpu_handle_resource_create_blob` **zeroes the whole buddy block** it is handed
(`write_bytes(phys_to_virt(p), 0, (1<<order)*4096)`), as does `DrmDumbBuffer::create`. So
the moment the freed block comes back round, the pattern is destroyed.

There is a second, independent destroyer that needs no reallocation at all:
`mm/src/buddy.rs::push_front` writes next/prev **into the first 16 bytes of the block it
pushes** (`node_set_next(addr, …)`, `node_set_prev(addr, …)`), and `free` always ends in a
`push_front`. Tracing the realistic case: a fresh `alloc(2)` with an empty order-2 list
walks up, splits, and returns the *base* while pushing the upper buddies — so the blob is
the lower half, its order-2 buddy is on `lists[2]`, and `free(x, 2)` coalesces (removing
that buddy), yields `addr = min(x, buddy) = x`, and `push_front(lists, 3, x)` writes at
offset 0 of the blob. The subsequent `alloc(2)` then pops `lists[3].head == x`, splits, and
returns `x` — which creation zeroes. Both mechanisms fire at offset 0 first, which is why
the check starts there and reports the first mismatching offset.

Neither mechanism is *individually* guaranteed (coalescing direction depends on which half
the block was, and 8 churn iterations only cover up to 8 pre-existing free order-2 blocks),
which is why the test carries **both** a pattern check over the entire buffer and, where
the kernel supports it, a deterministic counter check. They fail independently.

### 4.3 Does it fail at HEAD? — checked, and the gating was changed because of it

The design doc asserts Stage 2's test "fails at HEAD by construction". My first draft
**did not**, and I only found that by checking: I had gated the whole blob half behind the
new `VIRTGPU_PARAM_LEANDROS_BLOB_OBJS` getparam, which an unfixed kernel does not have —
so on HEAD it would have failed with "param missing", not with the hazard.

Fixed: `have_objs` now gates **only** the four counter assertions. The payload assertions —
the ones that read the recycled memory — run unconditionally. On an unfixed kernel the
test prints `(no BLOB_OBJS getparam - counter assertions skipped, payload assertions still
run and are the real gate)` and then FAILs `phase6_payload_survives_close`,
`phase6_mmap_of_fd_still_coherent` and `phase6_dumb_payload_survives_destroy`. The dumb
half never depended on the getparam and fails at HEAD unconditionally.

### 4.4 The mutation that flips it — name the line, trace the deletion

**The line:** `drivers/src/drm_device_interface.rs`, in `prime_export_acquire`:

```rust
o.refs = o.refs.saturating_add(1);      // blob arm
b.refs = b.refs.saturating_add(1);      // dumb arm
```

Delete the blob one. Trace:

1. `CREATE_BLOB` → `BlobObj{refs:1}` + handle. Unchanged.
2. `PRIME_HANDLE_TO_FD` → `prime_export_acquire` resolves and returns `obj` but does not
   increment. `install_dmabuf_vmo` still records `dmabuf_obj`.
3. `GEM_CLOSE` → `free_blob_owned` removes the handle → `blob_unref(obj, ctx)` → `refs`
   1→0 → object removed, `resource_unref`, `hostvis_free`, **`mm::buddy::free(phys, order)`**.
   → `phase6_objs_survive_close` FAILs: reads `objs0` where `objs1` was expected.
4. Churn: 8 × create, each `alloc(2)` + zero of 16 KiB. The freed block re-enters and is
   zeroed.
5. `read(fd)` → the VMO's `pages` still list those frames; `vmo_copy_out` reads through the
   HHDM. → `phase6_payload_survives_close` FAILs at the first mismatching offset;
   `phase6_mmap_of_fd_still_coherent` FAILs the same way.
6. `close(fd)` → `vmo_free_slot` returns `Some(obj)` → `bo_release_exported(obj)` → not in
   `BLOB_OBJS`, not in `DUMB_BUFFERS` → **serial log gets `[DRM] bo refcount underflow
   obj=…`**. A third, independent signal.
7. `phase6_objs_zero_after_fd_close` still PASSes — correctly; it is the leak-direction
   guard and the mutation is not a leak.

One deleted line, three named assertions plus a log line. The dumb twin is the same trace
through `free_dumb`.

**The reverse mutation** (make `blob_unref` decrement but never take the zero arm) flips
`phase6_objs_zero_after_fd_close` and nothing else, which is what that assertion is for.

### 4.5 How each failure presents, for triage

* `phase6_objs_survive_close` — **wrong value**, no error, no crash. Prints
  `after GEM_CLOSE: expected N live objects, got M`.
* `phase6_payload_survives_close` / `phase6_mmap_of_fd_still_coherent` /
  `phase6_dumb_payload_survives_destroy` — **wrong value**. `read()` returns the full
  count and the bytes are wrong (zeros, or free-list pointers). Prints
  `payload lost at offset N - the fd read RECYCLED memory`. It **must not panic**: the
  frames are still mapped in the HHDM, they merely belong to someone else now. A panic
  here would mean something *else* is wrong.
* `phase6_objs_zero_after_fd_close` — the opposite bug: the reference is never dropped and
  every export leaks a buffer.
* `phase6_alloc_after_release` / `phase6_dumb_alloc_after_release` — a **double**
  `mm::buddy::free` of an order-N block reached the allocator, which is corruption rather
  than a leak. The next allocation is the cheapest detector available.
* **Also check the serial log for `[DRM] bo refcount underflow`** on any phase-6 red. The
  test binary cannot see it, and it distinguishes "released twice" from "never taken".

### 4.6 The counter

`VIRTGPU_PARAM_LEANDROS_BLOB_OBJS = 0x1000_0005` reports `BLOB_OBJS.len()`, in the same
style and for the same reason as `LEANDROS_CTX_ID` / `LEANDROS_HOSTVIS_SPANS`: the count is
copied out of the guard into a local before the user pointer is touched, and no device
lock is involved. It counts **objects**, not handles and not fds — which is the only thing
that can distinguish "the fd kept the buffer alive" from "the buffer was freed and the read
happened to find plausible bytes".

---

## 5. Build results

RELEASE only, both arches, from `a0f2c46 + PRIME + this patch`.

| Target | Command | Result |
|---|---|---|
| kernel aarch64 | `cargo build -p kernel --target targets/aarch64-unknown-kernel.json --release -Zbuild-std=…` | **exit 0** |
| kernel x86_64 | same with `x86_64-unknown-kernel.json` | **exit 0** |
| userland aarch64 | `./scripts/build-userland.sh --release` | **exit 0**, `Build complete in userland/target/aarch64-unknown-none/release` |
| userland x86_64 | `./scripts/build-userland.sh --target amd64 --release` | **exit 0**, `Build complete in userland/target/x86_64-unknown-none/release` |

**Warnings.** `rustc` warning multiset for `drivers`, `vfs-server`, `kernel` and
`venustest` is unchanged from base; nothing new.

**rustfmt** — all five touched files *parse* cleanly (`rustfmt --emit stdout`, rc 0). No
reformatting was applied; `cargo fmt --check` on pristine HEAD emits thousands of lines, so
it is not a gate.

**clippy** — `cargo clippy` cannot build at HEAD: `sched` fails with two hard errors
(`this public function might dereference a raw pointer but is not marked unsafe`,
deny-by-default), confirmed. Working around it with `RUSTFLAGS=--cap-lints=warn` to get
past the dependency, the diagnostic multiset for `drivers` + `vfs-server` base-vs-mine
differs by exactly:

```
<    1 warning: `vfs-server` (lib) generated 38 warnings
>    1 warning: `vfs-server` (lib) generated 37 warnings
<    7 warning: this `if` statement can be collapsed
>    6 warning: this `if` statement can be collapsed
```

i.e. **zero added, one removed**. Two clippy hits I *did* introduce were found and fixed
during the pass (`the loop variable i is used to index rbuf` ×2 in venustest; a
`doc list item without indentation` ×2 in `BlobObj`). One remains and is deliberate:
venustest gains an eleventh `manually constructing a nul-terminated string`
(`open(b"/dev/dri/card0\0")`), which is the file's own established idiom — phase 5 does
exactly the same thing ten lines up, and there is no `CStr` in scope in this `no_std`
binary.

---

## 6. Stacking — verified by applying, not by inspection

Against `a0f2c46 + PRIME + this patch`:

| Patch | Result |
|---|---|
| `m9-fb-damage-clips/fb_damage_worktree_20260806.patch` | **applies cleanly** (plain `git apply`) |
| `m9-simulate-syncobj/simulate_syncobj_respec_20260806.patch` | needs `git apply -3`; then **clean, and the kernel + venustest both compile** |
| `m9-blob-cacheability/blob_cacheability.patch` | needs `git apply -3` **and a one-line reconcile** — see below |
| `m9-small-fixes/small_fixes.patch` | fails, **but it fails identically on `a0f2c46 + PRIME` without my patch** (`Cargo.lock`, `Cargo.toml`, `README.md`, `servers/init/*`). It is stale against `07d461c`, which deleted the init-server crate. Not mine. |

Both `-3` cases are **context** conflicts caused by my renames, not semantic ones:
`simulate_syncobj` carries a context line reading ``on `BlobBuf` `` which is now
``BlobObj``, and a venustest `extern "C"` hunk next to my `munmap` declaration. `git apply
-3` resolves both and the result compiles (checked: kernel aarch64 and venustest, no
errors).

**`blob_cacheability` needs one real edit.** Its `blob_map_cache_type(phys)` iterates
`BLOB_BUFFERS.lock().values()` reading `b.map_phys`, `b.size`, `b.map_info` — all of which
now live on the object. The reconcile is:

```rust
// LOCKING: takes BLOB_OBJS and nothing else, …
    // The OBJECT map: `map_phys`/`size`/`map_info` are per RESOURCE, and after
    // the object/handle split the handle map carries none of them.
    let blobs = BLOB_OBJS.lock();
```

I applied that reconcile and confirmed the kernel compiles with `blob_cacheability`
stacked on top of mine. Whoever lands the two together should take that edit; the working
tree is left with only my patch, so the reconcile is documented here rather than shipped.

---

## 7. `MODE_DESTROY_DUMB` — my judgment: **out of scope, and it should stay out**

The design doc (§1.2) is right that Mesa's kms_swrast importer destroys imported handles
with `DRM_IOCTL_MODE_DESTROY_DUMB`, not `GEM_CLOSE`
(`kms_dri_sw_winsys.c:288-296`), and that our `std_handle_destroy_dumb` takes no `open_id`
and consults no blob registry. But the leak it describes — one object per composited frame
— **requires an imported blob handle to exist**, and `PRIME_FD_TO_HANDLE` does not mint one
today: it echoes the exporter's handle verbatim (`kernel/src/syscall.rs`). There is
therefore currently no handle for `DESTROY_DUMB` to leak.

Folding it in now would mean giving `DESTROY_DUMB` an `open_id` and a `free_blob_owned`
call for a code path nothing reaches, and — worse — it would be **untestable**: the
assertion that proves it (design §5 Stage 3, guard 5: `MODE_DESTROY_DUMB(fdB, handleB)`
leaves the count at 1) needs importer handles, i.e. Stage 3. A guard-test-shaped change
with no test that can fail is exactly what this lane was told to avoid.

What I did instead: `std_handle_destroy_dumb`'s doc comment now records the gap, names the
Mesa line, and states that it becomes due with Stage 3. It is a two-line change when Stage
3 lands, in the same function that will already be edited.

---

## 8. Things I changed that were not asked for, and why

* **`free_blob`'s lock scope** (§2.4) — a real latent bug in code I was rewriting anyway.
* **`hostvis_map_blob`'s rollback keys on the object, not the handle.** With one handle per
  object this was equivalent; with two it is not. If the check stayed "did the HANDLE
  vanish", a concurrent `GEM_CLOSE` of one handle while another still held the object would
  undo a map the surviving handle is entitled to and `hostvis_free` a span the object's own
  teardown would free again. This is dead code until Stage 3 mints second handles, but it
  is the exact spot the design doc (§3.1) flags, and getting it wrong later would resurrect
  the window-span leak the current code exists to avoid.
* **`bo_attach_fence` writes the fence on the object.** Upstream attaches to the
  `virtio_gpu_object`; two handles naming one buffer must not disagree about whether work
  against it retired.
* **`blob_unref` skips `VIRTIO_GPU` when there is nothing to say** — this is now on the
  compositor's per-frame path.

---

## 9. Open, recorded, not fixed

1. **`mm::buddy::free` does not consult `mm::pageref`.** A process that `mmap`s its own
   dmabuf, closes the fd and drops the last handle still ends up with a live `MAP_SHARED`
   mapping of freed frames. Pre-existing on the dumb path, unchanged by this design, and
   the design doc's §2.5 / §9.2 says it deserves its own item. Phase 6 sidesteps it by
   `munmap`ping before `close(fd)`, with a comment saying why.
2. **Dumb handle numbers can eventually collide with the blob space.**
   `DrmDumbBuffer::next_handle` starts at 1 and increments; `NEXT_BLOB_HANDLE` starts at
   `0x4000`. A compositor that reallocates its scanout buffer per frame reaches 16384 in
   tens of minutes. Pre-existing; my change does not alter the allocation rate, only how
   long a *record* lives.
3. **`handle_ioctl_mmap` validates the mmap token globally, not per open** (design §9.3).
   Untouched. One narrow widening from this patch: a blob object kept alive solely by an
   fd is still matched by that scan, so a caller that saved the token before `GEM_CLOSE`
   can still `mmap` it on the card fd while the fd is open. The memory is alive and the
   caller had legitimate access to it, so this is not a safety change — but it is a
   change, and it is recorded here.
4. **The compositor steady-state measurement the design doc asks for is NOT done** — I
   could not run QEMU. Keeping a *dumb* buffer alive until its export fd closes changes
   cosmic-comp's steady state, because it exports a dmabuf per frame. Whoever runs this
   should watch for `DUMB_BUFFERS` / `BLOB_OBJS_LIVE` climbing monotonically over a 60 s
   session. **Note the direction of the risk**: this change can only ever make a buffer
   live *longer*, never shorter. Its failure mode is a leak; the failure mode it replaces
   was memory corruption. If the counts climb, that is information (Mesa is holding fds
   whose buffers we were previously freeing underneath it), not a reason to revert.

---

## 10. What still needs a machine

Everything. Nothing here was run. The gates the design doc names for Stages 1–2, unchanged:
`venustest` (now with phase 6), `vkrender` `s2_checksum = 0x02C0FDC5`, `vktest` 0 failures,
`drmsmoke` 22/0, `scmtest` 30/0, both arches, fresh images, `vfstest` run exactly once.
Plus the serial-log check for `[DRM] bo refcount underflow`, and the 60 s
compositor-steady-state check in §9.4.
