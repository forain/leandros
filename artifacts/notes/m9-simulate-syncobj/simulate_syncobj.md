# TODO item 6 — `SIMULATE_SYNCOBJ`: the rejected zero-size probe, and the `close(0)` it hides

Lane D, worktree based on `a0f2c46`. Status: **analysis complete, patch prepared, both
arches built**. QEMU not run (another lane owns it).

---

## 1. Source-level facts established first (all verified, not assumed)

Mesa tree read: `~/code/leandros-artifacts/llvmpipe-lane/src/mesa/src/virtio/vulkan/`.

### 1.1 The probe

`sim_syncobj_create` (`vn_renderer_virtgpu.c:145-190`) lazily submits, once per
process, on the *first* syncobj creation:

```c
struct drm_virtgpu_execbuffer args = {
   .flags = VIRTGPU_EXECBUF_RING_IDX | VIRTGPU_EXECBUF_FENCE_FD_OUT,
   .ring_idx = 0, /* CPU ring */
};
int ret = drmIoctl(gpu->fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &args);
if (ret || args.fence_fd < 0) { ...tear down, return 0; }
sim.signaled_fd = args.fence_fd;
```

Everything else in the struct — `size`, `command`, `fence_fd` — is left at the
designated-initialiser zero. We reject at `drm_device_interface.rs:2942`
(`exec.command == 0 || exec.size == 0`).

### 1.2 Mesa only ever polls the fd — confirmed

The *only* consumers of `sim.signaled_fd` / `pending_fd` are:

- `sim_syncobj_poll` (`:218-236`): `poll(&pollfd, 1, timeout)` with `.events = POLLIN`.
  Never `read(2)`. So an eventfd created with `initval = 1` stays at 1 forever and
  stays permanently readable — exactly the "already signalled" semantics wanted.
- `os_dupfd_cloexec` in `sim_syncobj_submit` (`:339`) and `sim_syncobj_export` (`:428`)
  — i.e. `fcntl(fd, F_DUPFD_CLOEXEC, ...)`.
- `close(2)`.

That is the whole surface. **An eventfd satisfies it.** (`F_DUPFD_CLOEXEC` is
implemented — `servers/vfs/src/lib.rs:4121,4128`, and `dup_vnode` bumps `EVENTFD_REFS`
at `:1044`, so a dup'd eventfd is correctly refcounted.)

### 1.3 `submit_3d` is synchronous — confirmed

`VirtioGpu::submit` (`drivers/src/virtio_gpu.rs:793-921`) busy-spins
`while q.last_used_idx == (*q.used).idx` until the host retires the chain, then sets
`self.last_completed_fence = fence_id`. `submit_3d` (`:1851`) goes through
`submit_checked(..., fenced = true, ...)`. So by the time `EXECBUFFER` returns to
userspace the work is **already retired**. venustest's own comment at
`userland/venustest/src/main.rs:1225` says the same ("submission is a synchronous
busy-spin, so every fence is already retired").

Therefore handing back a *pre-signalled* fence fd is not an approximation — it is
exact. There is no window in which the fd is signalled but the work is not done.

### 1.4 The `close(0)` is real, and it is currently *gated behind the bug we are fixing*

This is the finding that decides the shape of the patch, so it is stated in full.

`sim_submit` (`:517-560`):

```c
struct drm_virtgpu_execbuffer args = {
   .flags = VIRTGPU_EXECBUF_RING_IDX |
            (batch->sync_count ? VIRTGPU_EXECBUF_FENCE_FD_OUT : 0),
   ...
};
ret = drmIoctl(gpu->fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &args);
if (ret) { ...break; }
if (batch->sync_count) {
   ret = sim_submit_signal_syncs(gpu, args.fence_fd, ...);
   close(args.fence_fd);          /*  <-- fence_fd is still 0 today  */
   ...
}
```

`batch->sync_count != 0` requires a `vn_renderer_sync` to exist, which requires
`sim_syncobj_create` to have succeeded, which requires the probe in §1.1 to have
succeeded. **It does not succeed today.** So on HEAD the `close(0)` is unreachable —
the probe rejection is what hides it.

The two live callers of `vn_renderer_sync_create` (grepped, Mesa 25.3.6):

| call site | when |
|---|---|
| `vn_renderer_util.c:16` ← `vn_ring.c:388` | ring teardown, i.e. every `vkDestroyInstance` |
| `vn_queue.c:1824` `vn_create_sync_file` | sync-fd export (WSI, `VkExportFence`/`Semaphore` fd) |

Two consequences:

1. **Today's silent damage.** `vn_ring_destroy`'s `vn_renderer_submit_simple_sync`
   bails *before submitting* when the sync cannot be created, so the ring-destroy
   command is never sent. Every Venus instance leaks its host-side ring at teardown.
   That is a live defect on HEAD, not a hypothetical.
2. **The trap.** Accepting the probe *without* also writing `fence_fd` on the real
   submit path would ARM the `close(0)` on every `vkDestroyInstance`. A partial fix
   here is strictly worse than no fix. The patch therefore treats "accept zero-size"
   and "write `fence_fd` whenever `FENCE_FD_OUT` was asked for" as one indivisible
   change, and the latter is implemented at a layer that cannot be bypassed by the
   former.

---

## 2. The fd-plumbing finding (the main unknown in the brief)

**The plumbing exists, and the precedent is exact — but it is NOT in the `drivers`
crate, and it cannot be.**

`drivers/src/drm_device_interface.rs` runs inside the DRM server, which has no fd-table
access at all. The one place that has the caller's `pid`, the caller's address space,
and both the `drivers` and `vfs` crates in scope is `kernel/src/syscall.rs::sys_ioctl`.
That is precisely where `DRM_IOCTL_PRIME_HANDLE_TO_FD` was already intercepted
(`kernel/src/syscall.rs:6040-6115`), with the comment:

> The DRM server (drivers crate) has no fd-table access; building a dmabuf fd is a VFS
> op. This syscall layer runs in the caller's AS with pid + both the `drivers` and
> `vfs` crates in scope, so it is the one place that can turn a GEM handle into a real
> fd.

So the recorded "`fence_fd` blocked on fd plumbing" note is **stale**. Nothing new has
to be built: `sys_ioctl` can mint an eventfd with one `vfs::handle(VFS_EVENTFD, ...)`
message, exactly as `sys_eventfd2` (`kernel/src/syscall.rs:7012`) does. The ~40-line
estimate holds for the mechanism; the patch is larger only because of the regression
subtests.

The interception is keyed on the ioctl *number* (`0xC0406442`), which is unambiguous,
and must be placed before the generic `is_drm` forward at `:6123`.

---

## 3. The design decision: ACCEPT the probe, and mint the fd one layer up

### 3.1 Accept, not reject-with-fence_fd-written

Rejecting is not a viable option, for three independent reasons:

1. **It does not fix anything.** `sim_syncobj_create` checks `if (ret || args.fence_fd < 0)`.
   A non-zero `ret` fails the probe regardless of what `fence_fd` holds, so
   `sim.syncobjs` stays NULL, `vn_renderer_sync` stays dead, and the ring-teardown
   leak in §1.4 stays.
2. **It would leak.** On a non-zero `ret` Mesa never reaches `close(args.fence_fd)`.
   An fd written on a rejected probe is an eventfd nobody will ever close — and the
   probe is retried on *every* subsequent syncobj creation, since Mesa re-tests
   `if (!sim.syncobjs)`. That converts a one-shot failure into an unbounded fd leak.
3. **Accepting is semantically exact, not a fudge.** §1.3: submission is a busy-spin,
   so a fence over an empty stream is genuinely already retired.

Accepted shape is deliberately narrow: **`command == 0 && size == 0` only**. `size`
without `command` and `command` without `size` remain malformed and remain refused
(`phase7_half_zero_execbuffer_still_refused` pins that). `bo_handles` validation still
runs before the fence-only early return, so a fence-only request naming a bogus BO is
refused exactly as a real one is. `ctx_record_fence` is deliberately *not* called: a
fence id never sent to the host would make a later `VIRTGPU_WAIT` report on a
submission that does not exist.

### 3.2 The invariant that replaces the bug

> Whenever the caller set `FENCE_FD_OUT`, `fence_fd` is written before we return —
> a real fd (`>= 3`) on success, **`-1` on every failure path**. It is never left
> holding the caller's incoming value.

`-1` matters as much as the success value. Mesa checks `ret` before closing, but the
invariant must not depend on a caller being careful: `close(-1)` is a harmless `EBADF`,
`close(0)` is stdin. `phase7_failed_submit_writes_minus_one` is the guard for it.

### 3.3 Where the code lives, and why the split

| layer | file | responsibility |
|---|---|---|
| DRM device interface | `drivers/src/drm_device_interface.rs:2948-2968, 3100-3109` | accept the fence-only shape; **nothing** about fds |
| syscall | `kernel/src/syscall.rs:6123-6202` | reserve / write / release the eventfd |

The syscall layer sits *above* the accept, so there is no ordering in which a
fence-only request is accepted while `fence_fd` goes unwritten — the §1.4 trap is
structurally impossible rather than avoided by discipline. The divergence logger's
`ignored_fence_fd` was also narrowed to FENCE_FD_IN (plus a stray non-zero incoming
`fence_fd`), because claiming FENCE_FD_OUT is unhonoured is now a false report.

Ordering inside `sys_ioctl` follows upstream `virtio_gpu_execbuffer_ioctl`: reserve the
out fence **before** submitting, so a submission is never charged for an fd-table
failure, and release it if the submission fails.

**No FENCE_FD_OUT, no interception work**: the request is forwarded verbatim, so every
existing caller (all of which pass `flags = 0`) takes a byte-identical path.

---

## 4. fd lifetime — who closes what, and why repeated submits do not leak

| fd | created by | closed by | steady-state cost |
|---|---|---|---|
| probe fd (`sim.signaled_fd`) | one per **process**, first syncobj creation | **never** — Mesa keeps it for the life of the renderer | 1 eventfd slot per Venus process |
| per-batch out fence | one per `sim_submit` batch with `sync_count != 0` | Mesa: `close(args.fence_fd)` immediately after `sim_submit_signal_syncs` | 0 — transient |
| `syncobj->pending_fd` | `os_dupfd_cloexec` of the above | `sim_syncobj_set_point_locked` / `update_point_locked` / `destroy` | 1 per un-retired sync |
| reserved-then-failed | `sys_ioctl` | `sys_ioctl` itself, before returning the error | 0 |

The pool is `MAX_EVENTFDS = 256`, global (`servers/vfs/src/lib.rs:1126`). The only
*permanent* consumer is one fd per Venus process, which is Mesa's design, not our leak.
Everything else is transient or bounded by in-flight syncs. `EVENTFD_REFS`
(`:1044`, `:3438`) already refcounts dups correctly, so Mesa's `os_dupfd_cloexec` /
`close` pair cannot free a slot out from under a surviving dup — that hazard was
already closed by the calloop-aliasing fix.

`EFD_CLOEXEC` is set, matching upstream's `get_unused_fd_flags(O_CLOEXEC)` and Mesa's
own `os_dupfd_cloexec`: a fence fd must not survive into an exec'd child.

The residual risk is a *third-party* client that sets `FENCE_FD_OUT` and never closes
the fd. That is the same contract as any fd-returning ioctl (PRIME included), and today
there are no such callers: Mesa is the only user of `FENCE_FD_OUT` in the tree.

---

## 5. The regression subtests, and why they cannot pass vacuously

`userland/venustest/src/main.rs`, new `phase7_simulate_syncobj`, 11 subtests, gated on
the same `ctx_ok` as the rest of phase 5 (EXECBUFFER is refused before CONTEXT_INIT).
**venustest 68 → 79.** Stacked under item 5's PRIME patch it is 77 → 88.

Nine are **[GUARD]** — they fail against an unpatched kernel. Two are **[NON-REGR]** and
are labelled as such in the source; they are *not* counted as guards.

### 5.1 The falsifiability argument, subtest by subtest

The unpatched kernel has exactly two relevant properties, both checkable at HEAD
(`a0f2c46`) without running anything:

* **(U1)** `drm_device_interface.rs:2942` — `if exec.command == 0 || exec.size == 0
  { return Err(InvalidParameter); }`. The probe (`size == 0`, `command == 0`) is
  refused, always.
* **(U2)** Nothing anywhere writes offset 28 of `drm_virtgpu_execbuffer`. `grep` for
  writes through `fence_fd` in `drivers/` finds none: the field is read (for the
  divergence log) and never written. So on *every* path, `fence_fd` comes back holding
  whatever the caller put in.

Every guard seeds `fence_fd = 0` — Mesa's own designated-initialiser value — so (U2)
means the returned value is 0 on an unpatched kernel.

| subtest | asserts | unpatched result | why it must fail |
|---|---|---|---|
| `phase7_syncobj_probe_accepted` | `rc == 0` | `rc == -1` | (U1) |
| `phase7_syncobj_probe_fence_fd_written` | `fence_fd >= 3` | `0` | (U2) |
| `phase7_syncobj_probe_fd_signalled` | `fence_fd >= 3 && poll(POLLIN)` | short-circuits false | (U2) |
| `phase7_syncobj_probe_fd_dupable` | `F_DUPFD_CLOEXEC` gives a distinct, signalled fd | `dup_ok` stays false | (U2) |
| `phase7_submit_fence_fd_out_written` | `rc == 0 && fence_fd >= 3` | `rc == 0`, `fence_fd == 0` | (U2) — **and this is the literal `close(0)` denial** |
| `phase7_submit_fence_fd_signalled` | `poll(POLLIN)` on that fd | else-branch reports `false` | (U2) |
| `phase7_fence_fd_recycled_over_64_submits` | `first >= 3 && last == first` | `first == 0` | (U2) |
| `phase7_failed_submit_writes_minus_one` | `rc != 0 && fence_fd == -1` | `rc != 0` but `fence_fd == 0` | (U2) |
| `phase7_failed_submit_releases_fence_fd` | next success reuses `first` | `first == 0` guard fails | (U2) |
| `phase7_no_fence_fd_when_not_requested` | **[NON-REGR]** sentinel survives | passes | guards *this patch* against over-allocating |
| `phase7_half_zero_execbuffer_still_refused` | **[NON-REGR]** both half-zero shapes refused | passes | pins that the accept did not widen |

### 5.2 The trap I walked into, and did not ship

The obvious guard is the literal consequence: `close(args.fence_fd)` with `fence_fd == 0`,
then check stdin. **That guard cannot fail.** `sys_fcntl` (`kernel/src/syscall.rs:4947-4974`)
short-circuits `fd <= 2` and answers `F_GETFD` with a hardcoded `0` — it never consults
the fd table — so `fcntl(0, F_GETFD)` reports "stdin fine" whether or not stdin was
closed. That is precisely the hazard-window-never-opens failure mode this repo was burned
by, so the close-consequence test is **deliberately absent** and the reason is written
into the source comment on `phase7_simulate_syncobj`. The property is asserted directly
instead: `fence_fd >= 3` on success and `== -1` on failure means `close(fence_fd)` can
never name a stdio descriptor, whatever the caller does with it.

### 5.3 Why the leak guards are exact, not approximate

`ProcFdTable::alloc_fd` (`servers/vfs/src/lib.rs:1253-1262`) is lowest-free-from-3.
Nothing inside the 64-iteration loop opens an fd. Therefore, on a correct kernel, every
iteration returns the **same** number, and `last == first` is an exact expectation. A
fix that leaked the eventfd on close, or that failed to release the reservation on a
failed submit, makes the numbers climb and the subtest fails. `phase7_failed_submit_
releases_fence_fd` runs 8 deliberately-failing submits and then requires the next
success to come back at `first_fd` — which is only true if all 8 reservations were
released.

### 5.4 What these subtests do NOT cover

* FENCE_FD_IN — still unhonoured, still logged as a divergence.
* The host-side effect of a fence-only submission: there is none by construction (we
  submit nothing), which is the point, but it means the subtests prove a guest-side
  ioctl contract only, exactly like the rest of venustest.
* Whether Mesa's `sim_submit` now actually runs to completion. That needs `vktest` /
  `vkrender` on a Venus host and cannot be checked on the Mac.

---

## 6. Build results

`./scripts/build-all.sh --arch <a>`, release, run twice per arch (the second run after
making `PollFd` `pub` to silence a new `private_interfaces` warning).

| arch | exit | notes |
|---|---|---|
| aarch64 | **0** | f2fs image built, 2 034 237 440 bytes |
| x86_64 | **0** | f2fs image built, 2 042 626 048 bytes |

`venustest` compiles with **2 warnings on both arches, both pre-existing** (`c_uint`
unused; `VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs` casing). No new warning in
`kernel/src/syscall.rs` or `drivers/src/drm_device_interface.rs`.

**Not run in QEMU** — another lane owns it.

---

## 7. Stacking check (done by applying, not by inspection)

Patch: `simulate_syncobj.patch`, 3 files, +383/−8, `git apply --check`-clean at `a0f2c46`
and cleanly reverse-appliable.

| combination | result |
|---|---|
| mine on clean `a0f2c46` | OK |
| mine → `blob_cacheability.patch` | OK |
| `blob_cacheability.patch` → mine | OK |
| mine → `prime_handle_to_fd_built_20260806.patch` | OK |
| `prime_handle_to_fd_built_20260806.patch` → mine | OK |
| mine → `fb_damage_worktree_20260806.patch` | OK |
| `fb_damage_worktree_20260806.patch` → mine | OK |
| blob + prime + fb_damage, then mine | OK |

**Order-independence proven, not assumed** for the contended pair: PRIME touches all
three of the same files I do (`drm_device_interface.rs`, `kernel/src/syscall.rs`,
`userland/venustest/src/main.rs`). Both application orders produce the *identical* tree
`4511a935b548969e0766f92b4dae3efa904e4761`.

The hunks are genuinely disjoint: PRIME's `sys_ioctl` hunks end at the
`DRM_IOCTL_PRIME_FD_TO_HANDLE` block and mine begin immediately after it; PRIME's
`drm_device_interface.rs` hunks are all below line 1260 and mine are at 2948/3062/3100;
PRIME appends its venustest phase at the end of `venus_main` while mine adds a helper
before `wait_bo` plus one call inside phase 5.

Note per the brief: `fb_damage_worktree_20260806.patch` may never land. It applies with
mine in both directions anyway, so its fate does not affect this work.

---

## 8. What is still open after this

* **FENCE_FD_IN** (sync_file import) is unimplemented. It needs the *reverse* plumbing —
  `sys_ioctl` resolving a caller fd to a fence and waiting on it before submission — and
  there is no signalled-by-construction shortcut for it. Mesa's SIMULATE path never sets
  it, so nothing is blocked today.
* **Real syncobj ioctls** (`DRM_IOCTL_SYNCOBJ_*`). Implementing them would let Mesa's
  non-SIMULATE path run; the SIMULATE path is what 25.3.6 compiles, so this is not on
  the critical path.
* **The eventfd is not a real fence.** It is signalled at creation. That is correct only
  while `submit` is a synchronous busy-spin. If submission ever becomes asynchronous
  (the ISR work in the Venus M2 notes), this becomes a lie and must become a real
  waitable fence object. The comment in `sys_ioctl` says so explicitly, and the
  dependency is on `VirtioGpu::submit`, not on anything in this patch.

