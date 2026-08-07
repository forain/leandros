# M9 — SIMULATE_SYNCOBJ subtest re-spec + `--present` (2026-08-06)

Lane M. Box `forain@172.16.158.150`, `/home/forain/Projects/leandros`, branch
`main` at `eccc4e9` (= `0df1810` blob cacheability + `e083202` PRIME export +
`eccc4e9` vkswap staging). Both stashes untouched. Nothing pushed to `origin`.

**Headline:** the previous lane's *fix* is right and now lands 91 PASS / 0 FAIL,
but its *diagnosis* of why the four subtests timed out is **wrong**, and I have
the wire trace that falsifies it. `VIRTGPU_EXECBUF_RING_IDX` with `ring_idx = 0`
is not the problem — Mesa issues exactly that, on a virgin context, before any
host-side ring exists, and it completes. The real variable is the **stream**.
Details in §2.

---

## 0. Harness and positive control

**Harness for every venustest/gate number below:** `/tmp/m9lane/lane_i_run.py`
on the box — the previous lane's copy of the corrected `m9b_run.py`. `mark()` is
taken before `send_line`, `expect(..., start=mk)` cannot see a byte that predates
the send, and the end sentinel is numbered per command (`RC=$? ZZEND<n>`).

**Harness for the `--present` run:** `/tmp/m9lane/m9m_present.py`, written for
this lane. Same pty/mark/sentinel discipline, plus a QMP client on a unix socket
so the screendump is taken *while* the guest is holding the image, from the same
single process. No watcher, no poller.

**Positive control — CMD[0] of every boot in this report:**

```
### CMD[0]: nosuchbinary_xyz42
### rc=127
```

Every count below is additionally cross-checked three ways: the literal
`<name>: PASS` / `<name>: FAIL` lines the test binary prints, the binary's own
`--- <name> done, failures = N ---` trailer, and the harness `rc`.

**Observables were chosen to be structurally distinctive, not counts.** The
decisive Mesa evidence in §2 is a `(payload size × flag word × ring index)`
tuple table whose every row is independently accountable — not a delta.

---

## 1. Task 1 — the four subtests: before / after

### Before (previous lane, run D, patch as submitted)

```
phase7_syncobj_probe_accepted: PASS
phase7_syncobj_probe_fence_fd_written: PASS
phase7_syncobj_probe_fd_signalled: PASS
phase7_syncobj_probe_fd_dupable: PASS
virtio_gpu_cmd_ctx_submit ctx 0x1, size 32
[GPU] control-queue TIMEOUT, cmd=0x00000207
phase7_submit_fence_fd_out_written: FAIL
phase7_submit_fence_fd_signalled: FAIL
phase7_fence_fd_recycled_over_64_submits: FAIL
phase7_failed_submit_writes_minus_one: PASS
phase7_failed_submit_releases_fence_fd: FAIL
phase7_no_fence_fd_when_not_requested: PASS
phase7_half_zero_execbuffer_still_refused: PASS
```

### After (this lane, `/tmp/m9lane/runDIAG-x86_64.log`, x86_64 Venus)

```
phase7_syncobj_probe_accepted: PASS
phase7_syncobj_probe_fence_fd_written: PASS
phase7_syncobj_probe_fd_signalled: PASS
phase7_syncobj_probe_fd_dupable: PASS
phase7_submit_fence_fd_out_written: PASS
phase7_submit_fence_fd_signalled: PASS
phase7_fence_fd_recycled_over_64_submits: PASS
phase7_failed_submit_writes_minus_one: PASS
phase7_failed_submit_releases_fence_fd: PASS
phase7_no_fence_fd_when_not_requested: PASS
phase7_half_zero_execbuffer_still_refused: PASS
--- venustest done, failures = 0 ---
```

`grep -c "control-queue TIMEOUT"` over that log: **0**.

**The total is 91, not 79.** 79 was the previous lane's number against stock
`0df1810`; `e083202` (PRIME) has since landed on the box's `main` and adds 12
phase-5 reports. 68 base + 12 PRIME + 11 phase-7 = **91 PASS / 0 FAIL**, which is
exactly the previous lane's 87/4 with the four flipped.

### The edit

Three call sites in `userland/venustest/src/main.rs`, all
`EXECBUF_RING_IDX | EXECBUF_FENCE_FD_OUT` → `EXECBUF_FENCE_FD_OUT`:

* subtest 2's `sub` (`phase7_submit_fence_fd_out_written` / `_signalled`)
* the 64-iteration recycling loop's `e`
* the final *successful* submit inside `phase7_failed_submit_releases_fence_fd`

The rule the edit follows, and which is written into the source: **a submission
that must reach the host and complete is unringed; a submission that is refused
before it ever reaches the host keeps the flag.** So `RING_IDX` survives on the
probe (answered by the fence-only early return, never submitted), on the eight
deliberately-failing bad-BO submits inside `_releases_fence_fd` (refused at
`bo_handles` validation), on `phase7_failed_submit_writes_minus_one`, and on both
half-zero shapes. Those five subtests were already green and were not touched.


---

## 2. Can the real Mesa path hit the same timeout? **No — and the recorded reason for the timeout is wrong**

The brief asked me to satisfy myself about this rather than paper over it. Doing
so overturned the previous lane's diagnosis.

### 2.1 What the previous lane concluded, and why it cannot be true

> "the host is asked to fence on a **per-context ring that the stream never
> created**; the fence is dropped and the guest spins to its timeout … Mesa never
> hits this because `vn_ring_create` really does create ring 0 before it ever
> submits with `RING_IDX`."

That last clause is false on its face, from Mesa's own source. Both of Mesa's
"simple submit" helpers hard-code ring 0 *as the CPU ring*, and they are what
issue the very first submission of a renderer lifetime — long before any
`vkCreateRingMESA` has reached the host:

`src/virtio/vulkan/vn_renderer_util.h:22-36` (`vn_renderer_submit_simple`)

```c
   const struct vn_renderer_submit submit = {
      .batches = &(const struct vn_renderer_submit_batch){
            .cs_data = cs_data,
            .cs_size = cs_size,
            .ring_idx = 0, /* CPU ring */
      },
```

`src/virtio/vulkan/vn_renderer_util.c:20-30` (`vn_renderer_submit_simple_sync`,
the one `vn_ring_destroy` uses) — note `sync_count = 1`, which is what makes
`sim_submit` set `FENCE_FD_OUT`:

```c
            .ring_idx = 0, /* CPU ring */
            .syncs = &sync,
            .sync_values = &(const uint64_t){ 1 },
            .sync_count = 1,
```

and `sim_submit` (`vn_renderer_virtgpu.c:534-543`) sets `RING_IDX`
**unconditionally**, on every batch:

```c
      struct drm_virtgpu_execbuffer args = {
         .flags = VIRTGPU_EXECBUF_RING_IDX |
                  (batch->sync_count ? VIRTGPU_EXECBUF_FENCE_FD_OUT : 0),
         ...
         .ring_idx = batch->ring_idx,
      };
```

So if a `RING_IDX` fence on a not-yet-created ring hung, Mesa would hang on its
*first* submission, every time. It does not.

### 2.2 The measurement that settles it

Rebuilt x86_64 with `drivers/src/drm_device_interface.rs:366` flipped to
`pub const GPU3D_DEBUG: bool = true;`, which turns on the existing `[EXECDBG]`
per-EXECBUFFER trace (`ctx / size / flags / ring / nbo / cmd`). One boot,
`venustest` then `vktest`, `--venus`, positive control `rc=127` first. Every
distinct `(size, flags, ring)` tuple in that boot, with its count:

| count | `size` | `flags` | `ring` | who |
|---|---|---|---|---|
| 66 | `0x20` | `0x02` | 0 | **venustest, the three fixed subtests** (1 + 64 + 1) |
| 14 | `0x20` | `0x00` | 0 | venustest phases 2–5 + `no_fence_fd_when_not_requested` |
| 9 | `0x20` | `0x06` | 0 | venustest's refused bad-BO submits (8 + 1) — kept `RING_IDX` |
| 5 | `0x18` | `0x04` | 0 | **Mesa** `vn_ring` notifies |
| 2 | `0x00` | `0x06` | 0 | the two `sim_syncobj_create` probes (venustest's + **Mesa's own**) |
| 1 | `0x8C` | `0x04` | 0 | **Mesa's FIRST submit of the lifetime** (140 B) |
| 1 | `0x10` | `0x06` | 0 | **Mesa's `vn_ring_destroy` teardown** (16 B) |

`flags`: `0x02` = `FENCE_FD_OUT`, `0x04` = `RING_IDX`. **Control-queue timeouts
in that entire boot: 0.**

Two rows kill the ring hypothesis outright:

* **`size=0x8C flags=0x04 ring=0`** — Mesa's *first* execbuffer of the renderer
  lifetime, a real 140-byte stream, carrying `RING_IDX` with `ring_idx = 0` on a
  context where **no host-side ring has ever been created** (this submission *is*
  the ring creation). It completes. Ring 0 is intrinsically valid host-side; it
  does not have to be created by the guest.
* **`size=0x10 flags=0x06 ring=0`** — `vn_ring_destroy`'s teardown submit:
  `RING_IDX | FENCE_FD_OUT`, ring 0, real stream. That is **byte-for-byte the
  flag word `phase7_submit_fence_fd_out_written` used to send**, and it completes
  in the same boot on the same kernel.

The accounting is exact and self-checking: 66 = 1 + 64 + 1 (the three edited
sites); 9 = 8 + 1 (the refused bad-BO submits, which kept the flag); 2 = the two
probes; `half_a` (size 32 / command 0) and `half_b` (size 0 / command non-null)
correctly do **not** appear, because both are refused above the trace point.

### 2.3 What the timeout actually was

The variable is the **command stream**, not the flag. venustest submits 32 zero
bytes, which is not a dispatchable Venus stream — the host says so in the same
log the previous lane quoted:

```
virgl_render_server[1171922]: vkr: vkCreateInstance resulted in CS error
virgl_render_server[1171922]: vkr: submit_cmd: vn_dispatch_command failed
virgl_render_server[1171922]: failed to dispatch context op 5
```

With `RING_IDX` set, the guest sets `VIRTIO_GPU_FLAG_INFO_RING_IDX`
(`virtio_gpu.rs:1866-1870`), so QEMU retires the completion fence through the
**renderer context** — `virgl_renderer_context_create_fence(ctx, …, ring_idx, …)`
— instead of the global timeline `virgl_renderer_create_fence()`. For a Venus
context whose command dispatch has failed, that context-routed fence is never
retired; QEMU defers the control-queue response until the fence retires, so the
descriptor is never returned and `VirtioGpu::submit`'s 100 M-iteration busy-spin
(`virtio_gpu.rs:890`) reports `control-queue TIMEOUT, cmd=0x00000207`. Unringed,
the fence lands on the global timeline and retires regardless of what the host
made of the bytes — which is exactly why every *other* synthetic submission in
venustest (phases 2, 3, 5) has always been unringed and has always passed.

The previous lane's own discriminator (`flags = 0` succeeds seconds later on the
same context) never separated "ring missing" from "stream rejected": `flags = 0`
takes the global-timeline path, which does not consult the renderer context at
all.

### 2.4 Therefore

**The real Mesa path cannot hit this timeout.** Reaching it requires the host to
fail to dispatch the submitted stream, and Mesa's streams are valid Venus
protocol — `vktest`, `vkrender` and `vkswap` issue dozens of `RING_IDX` submits
per boot, on ring 0 and on real rings, with zero timeouts. Nothing in the kernel
needs changing, and the brief's instruction not to "fix" `RING_IDX` kernel-side
stands (Mesa sets it on every submit; refusing or rewriting it would break Mesa).

### 2.5 The residual finding, stated at the strength the evidence supports

There *is* a standing robustness gap, and it is a slightly different shape from
the one the previous lane recorded, so its TODO wording should be corrected:

> A `RING_IDX`-routed SUBMIT_3D whose **command stream the host refuses to
> dispatch** is never completed: the response is deferred to a context fence the
> host will not retire, and the caller pays a full control-queue busy-spin
> timeout instead of receiving an error. It is a hang shape available to any
> client that submits a malformed stream with `RING_IDX`, which is every Mesa
> submission.

Not "a ring the guest never created" — ring 0 needs no creation, and I did not
test a *nonexistent* ring index (our driver bounds-checks `ring_idx` against the
context's `num_rings` before it can get that far). What I have **not** separated,
and am not claiming either way, is whether the non-retiring fence is a property
of *that* submission's failed dispatch or of a context already poisoned by an
earlier one — venustest's failing case always ran on a context that had already
had a stream rejected. Both readings give the same answer in §2.4, so I did not
spend a build cycle separating them.

---

## 3. Did the guard argument survive the edit? **Yes, unchanged — and the loss is named**

The must-fail-unpatched argument for all nine `[GUARD]` subtests rests on two
properties of the unpatched kernel, both stated in `simulate_syncobj.md` §5.1:

* **(U1)** `drm_device_interface.rs:2942` refuses `command == 0 || size == 0`.
* **(U2)** *Nothing anywhere writes offset 28 of `drm_virtgpu_execbuffer`*, so
  `fence_fd` comes back holding the caller's seed on every path.

The four re-specified subtests all rest on **(U2)** alone. `RING_IDX` appears
nowhere in that argument, and — decisively — it appears nowhere in the *fix*
either: the patched `sys_ioctl` block keys entirely on
`flags & EXECBUF_FENCE_FD_OUT` and never reads `ring_idx`. So the four still seed
`fence_fd = 0` (Mesa's designated-initialiser value), still demand `>= 3` on
success and `-1` on failure, and an unpatched kernel still returns 0 for all of
them. **Guard strength is bit-for-bit unchanged.**

**What IS lost, stated plainly:** `phase7_submit_fence_fd_out_written` no longer
sends `sim_submit`'s literal flag word. It asserts the fd contract for a
`FENCE_FD_OUT` submission; it no longer *also* demonstrates that the ringed
variant of that shape works.

**The compensating control, which is real and is in the same suite:** §2.2 row 7
— `size=0x10 flags=0x06 ring=0`, Mesa's `vn_ring_destroy` teardown submit — is
that exact ringed shape, issued by Mesa itself at every `vkDestroyInstance`, and
it completes. `vktest`, `vkrender` and `vkswap` all run in the gate below, so the
ringed path is covered by them rather than by a synthetic subtest that cannot
exercise it. This reasoning, including the loss, is written into the phase-7 doc
comment in the source so the next reader does not "restore" the flag.

I did **not** add a guard that cannot fail: no subtest was added, three flag
words were narrowed, and every one of the nine guards still returns 0 from
offset 28 on an unpatched kernel.

---

## 4. Non-regression — x86_64, fresh image, `vfstest` first and once

Build: box `main` (`eccc4e9`) + the syncobj patch with the phase-7 re-spec,
`./scripts/build-all.sh --arch x86_64`, exit 0, RELEASE. Fresh f2fs image.
`--venus`. Harness `lane_i_run.py`, log `/tmp/m9lane/runGATE-x86_64.log`.

```
### CMD[0]: nosuchbinary_xyz42
### rc=127
```

| test | result | required |
|---|---|---|
| `vfstest` | **36 PASS / 0 FAIL** | 36/0 ✅ |
| `scmtest` | **30 PASS / 0 FAIL** | 30/0 ✅ |
| `venustest` | **91 PASS / 0 FAIL** (`failures = 0`) | see §1 ✅ |
| `drmsmoke` | **22 PASS / 0 FAIL** | 22/0 ✅ |
| `vkrender` | **51 PASS / 0 FAIL**, `s2_checksum = 0x02C0FDC5` | 0 failures + pinned checksum ✅ |
| `vktest` | **14 PASS / 0 FAIL** (`SUMMARY: 0 failure(s)`) | 0 failures ✅ |
| `vkswap` | **21 PASS / 0 FAIL** | — ✅ |

Counts are from a per-command segmentation of the raw serial log between the
harness's numbered `ZZEND<n>` sentinels, cross-checked against each binary's own
`failures = N` trailer and the harness `rc`. Limine revision untouched.

---

## 5. Non-regression — aarch64, fresh image, `vfstest` first and once

Build: same tree, `./scripts/build-all.sh --arch aarch64`, exit 0, RELEASE, fresh
image. `--venus`, plus `-cpu max,lpa2=off` for the Limine 11.4.1 FEAT_LPA2 wedge.
Log `/tmp/m9lane/runGATE-aarch64.log`, positive control `rc=127` first.

| test | x86_64 | aarch64 | required |
|---|---|---|---|
| `vfstest` | **36 / 0** | **36 / 0** | 36/0 ✅ |
| `scmtest` | **30 / 0** | **30 / 0** | 30/0 ✅ |
| `venustest` | **91 / 0** | **91 / 0** | ✅ |
| `drmsmoke` | **22 / 0** | **22 / 0** | 22/0 ✅ |
| `vkrender` | **51 / 0** | **51 / 0** | 0 failures ✅ |
| `vkrender` `s2_checksum` | `0x02C0FDC5` | `0x02C0FDC5` | `0x02C0FDC5` ✅ |
| `vktest` | **14 / 0** | **14 / 0** | 0 failures ✅ |
| `vkswap` | **21 / 0** | *(no aarch64 binary)* | — |
| `control-queue TIMEOUT` | **0** | **0** | — |

The two arches are identical subtest for subtest, including all eleven phase-7
reports. `vkswap` has no aarch64 build — the box cannot cross-build it (no arm64
binfmt handler for the Alpine container); the previous lane recorded
`zig cc -target aarch64-linux-musl` as the route if it is wanted.

Release builds only. Limine revision untouched. Nothing pushed to `origin`; both
pre-existing stashes untouched and unpopped.

---

## 6. Task 2 — `--present`: it runs, and the blit reaches the scanout

`--present` turned out to be genuinely *unrun*, not unfinished. It needed no
code changes at all.

Run: `/tmp/m9lane/m9m_present.py --arch x86_64`, `--venus`, no COSMIC (the boot
lands on a login shell), QEMU trace events `virtio_gpu_cmd_*` enabled, QMP on a
unix socket. Positive control `rc=127` first. Log
`/tmp/m9lane/runPRESENT-x86_64.log`.

### Guest side — 10/10, and vkrender still 0 failures overall

```
[STEP] --present — blit the rendered image to a DRM dumb buffer and SETCRTC it
[INFO] present: this drives CRTC 1 / connector 1 with no master arbitration; COSMIC must not be running
present_open_card0: PASS
present_getresources: PASS
present_getconnector: PASS
[INFO] present: mode 1920x1080
present_create_dumb: PASS
present_map_dumb: PASS
[DRM-SRV] mmap token=0x0000000001800000 map_info=0x01 -> writeback
present_mmap: PASS
present_blit: PASS
present_addfb2: PASS
present_setcrtc: PASS
present_dirtyfb: PASS
[INFO] present: holding the image for 30000 ms — take the QMP screendump now
vkDestroyDevice: PASS
vkDestroyInstance: PASS

--- vkrender done, failures = 0, skipped = 0 ---
```

`rc=0`. Note that no arbitration warning fired and nothing else on the system
fought for the CRTC.

### Host side — the decisive evidence

The QEMU trace stream shows the guest's DRM present as a device-level scanout
switch. The console's own traffic is unmistakable (an 8x16 glyph cell flushed
per character on resource `0x1`), which makes the present stand out exactly:

```
virtio_gpu_cmd_res_create_2d res 0xb, fmt 0x1, w 1920, h 1080
virtio_gpu_cmd_res_back_attach res 0xb
   … (console keeps flushing res 0x1, w 8, h 16 …)
present_setcrtc: PASS
virtio_gpu_cmd_set_scanout id 0, res 0xb, w 1920, h 1080, x 0, y 0
virtio_gpu_cmd_res_xfer_toh_2d res 0xb
virtio_gpu_cmd_res_flush res 0xb, w 1920, h 1080, x 0, y 0
present_dirtyfb: PASS
```

That is the whole chain: a new 1920x1080 host resource is created and backed,
**scanout 0 is switched from the console resource `0x1` to the present resource
`0xb`**, the guest's dumb buffer is transferred to it, and the full frame is
flushed. When vkrender exits, the console driver takes it back —
`virtio_gpu_cmd_set_scanout id 0, res 0x1, …` — which is a second, independent
confirmation that the scanout really had been handed over.

**So yes: the blit reaches the scanout.** `SET_SCANOUT` + `TRANSFER_TO_HOST_2D` +
a full-frame `RESOURCE_FLUSH` on the presented resource is the same host-side
sequence any working display produces; there is nothing left between that and
photons except the host display backend.

### What could NOT be captured, and why

The one thing this host cannot give is a photograph of that resource. Bare
`screendump` works (TODO item 4 measured this correctly) and returns a valid
1920x1080 PPM, but its content is the **text console**, not the present:

```
before.ppm : 1920x1080  nonblack=44643 (2.15%)  0x181818=0 (0.00%)  distinct=3
during1.ppm: 1920x1080  nonblack=61648 (2.97%)  0x181818=0 (0.00%)  distinct=3
during2.ppm: 1920x1080  nonblack=61648 (2.97%)  0x181818=0 (0.00%)  distinct=3
    #000000 / #ffffff / #cd0000 only
```

Three colours — black, white, and brush's red — i.e. characters. `--present`
paints a `0x181818` field with the 256x256 render centred, so the presented
frame is *structurally* unmistakable, and **not one `0x181818` pixel appears**.
The screendump also tracked the console (2.15% → 2.97% ink as more text
scrolled), so it is live, not stale.

`screendump device=` on the GL device is refused — but not for the reason TODO
item 4 records. It is not `"no surface"`: QMP resolves `device` as a **qdev id**,
and `--venus`'s device line carries no `id=`, so a QOM path gets
`DeviceNotFound`. A second run with `id=venusgpu` added to `GPU_DEV` settles what
the real limit is; result in §6.1. (That edit to `scripts/run-qemu.sh` is
temporary and is reverted — it is not part of any commit.)

### 6.1 `screendump device=` — TODO item 4's "no surface" is only half right

Second run with `,id=venusgpu` added to `GPU_DEV` (temporary; reverted, not
committed) and traces sent to their own file (`-D`). Log
`/tmp/m9lane/runPRESENT2-x86_64.log`, traces `/tmp/m9lane/trace2-x86_64.log`,
shots `notes/m9-present/*.ppm`.

```
### screendump[before]   bare        -> {'return': {}}
### screendump[before]   gl(venusgpu)-> {'return': {}}          <- WORKS
### hold banner seen
### screendump[during1]  bare        -> {'return': {}}
### screendump[during1]  gl(venusgpu)-> {'error': {'class': 'GenericError', 'desc': 'no surface'}}
### screendump[during2]  bare        -> {'return': {}}
### screendump[during2]  gl(venusgpu)-> {'error': {'class': 'GenericError', 'desc': 'no surface'}}
```

So `device=` is **not** categorically broken — the earlier failure was
`DeviceNotFound`, because QMP resolves `device` as a qdev id and `--venus`'s
device line has none. Given an id it works *until* the guest sets a scanout, and
only then reports `"no surface"`.

`before-gl.ppm` says what that surface was: **1280x800**, 99.93% black plus 684
`#aaaaaa` pixels — Limine's boot-era surface, which the clean trace confirms
exactly:

```
virtio_gpu_cmd_res_create_2d  res 0x1, fmt 0x2, w 640,  h 480     <- Limine
virtio_gpu_cmd_set_scanout    id 0, res 0x1, w 640,  h 480
virtio_gpu_cmd_res_create_2d  res 0x2, fmt 0x2, w 1280, h 800     <- Limine
virtio_gpu_cmd_set_scanout    id 0, res 0x2, w 1280, h 800
   … LeandrOS boots …
virtio_gpu_cmd_res_create_2d  res 0x1, fmt 0x1, w 1920, h 1080    <- kernel console
virtio_gpu_cmd_set_scanout    id 0, res 0x1, w 1920, h 1080
   … vkrender --present …
virtio_gpu_cmd_res_create_2d  res 0xb, fmt 0x1, w 1920, h 1080    <- the dumb buffer
virtio_gpu_cmd_set_scanout    id 0, res 0xb, w 1920, h 1080       <- THE PRESENT
virtio_gpu_cmd_set_scanout    id 0, res 0x1, w 1920, h 1080       <- console takes it back
```

The GL device's `DisplaySurface` was never advanced past Limine's 1280x800, and
once a virgl-backed scanout is set it is replaced by a GL scanout with no
surface at all. **Under `-display egl-headless` + `virtio-gpu-gl-pci` there is no
capturable surface for the presented frame.** That is a host-tooling limit, not
a LeandrOS defect, and it is worth correcting in TODO item 4: the constraint is
"the GL console has no `DisplaySurface` once a scanout is set", not "`device=`
does not work".

### 6.2 Verdict on Task 2, and the one thing still missing

`--present` is **done and working as far as this host can be made to show**:
10/10 subtests, `vkrender` 0 failures overall, and a complete, correct
device-level scanout handover in the host trace. It needed no work beyond
running it.

What is *not* proven is the last hop — that the bytes in the presented resource
are the rendered triangle rather than garbage. The cheap way to close it, and
the only piece of item 4 left, is a **standalone dumb-buffer present tool with
no Vulkan dependency**, run on the **default (non-Venus) `run-qemu.sh` path**
where the GPU is a plain virtio device with a real `DisplaySurface`; bare
`screendump` then captures it and the `0x181818` field plus a known pattern is
trivially checkable. That separates "does the DRM present path put pixels on a
scanout" (answered: yes) from "does this Venus host have a photographable
display" (answered: no).

---

## 7. Committed locally on the box's `main`

```
a0325c6 drm: mint an out-fence fd for EXECBUFFER's FENCE_FD_OUT   <- this lane
eccc4e9 mkfs: stage vkswap when the venus artifact tree provides it
e083202 drm: export Venus blob handles through PRIME_HANDLE_TO_FD
0df1810 drm: honour the host's requested blob cacheability
```

`origin/main` is untouched at `6a0eb0c`. Both pre-existing stashes untouched and
unpopped. `scripts/run-qemu.sh` was temporarily edited for §6.1 and restored from
a backup before the commit; `git status` shows only the two pre-existing
untracked paths (`doomgeneric/`, `x86_64_vars_linux.fd`).

The landed change is exported to
`notes/m9-simulate-syncobj/simulate_syncobj_respec_20260806.patch`
(the original `simulate_syncobj.patch` is left alongside it, unmodified).
Logs, harnesses and screendumps: `notes/m9-present/m9-lane-m-logs.tgz` and
`notes/m9-present/*.ppm`.

---

## 8. Instruments — one more lied, and it is the same family as the other five

**Sixth liar: `grep` over a serial log that shares a pty with QEMU's trace
stream.** With `-trace virtio_gpu_cmd_*` and no `-D`, every guest character
triggers a console flush, so the trace lines land *between* the guest's bytes.
`present_addfb2: PASS` arrives as twenty single characters, each followed by a
trace line. `grep -a "present_"` found **2 of the 10** present subtests and
reported nothing wrong; the eight "missing" ones looked exactly like eight
subtests that had not run. Only dumping the raw region and reading the leading
character of each line revealed them.

The same shredding broke the **harness's own sentinel**: `RC=0 ZZEND1` is in the
log, plainly, after de-interleaving — but the raw buffer never contained those
bytes contiguously, so `expect()` timed out on a command that had succeeded. The
first present run therefore reports a harness failure over a `rc=0` result.

Fix, used for run 2: `-D <file>` so the trace stream never touches the pty. The
de-interleaving extractor (`re.sub(r"virtio_gpu_cmd_[^\n]*\n", "", log)`) is in
the archive for reading run 1.

**Nothing in §1, §4 or §5 rests on that**: the gate and diagnostic runs had no
host tracing, and every count there was taken three ways (grepped `: PASS` /
`: FAIL` lines segmented per command between the numbered `ZZEND<n>` sentinels,
the binary's own `failures = N` trailer, and the harness `rc`), which agreed.

**Positive control:** `nosuchbinary_xyz42` as CMD[0] of all five boots in this
report (diagnostic, x86_64 gate, aarch64 gate, present ×2). `rc=127` every time.

**The observable I chose for the decisive claim was deliberately not a count.**
The previous lane was burned by a submit-count delta of 6→7 that turned out to
float. §2.2's table is a `(payload size, flag word, ring index)` histogram: every
row is attributable to a specific caller, the totals reconcile exactly
(66 = 1 + 64 + 1, 9 = 8 + 1, 2 = 2 probes), and the two rows that carry the
argument — a 140-byte `RING_IDX` submit and a 16-byte `RING_IDX | FENCE_FD_OUT`
submit — are qualitatively unique, not marginal.

---

## 9. Verdicts

### Task 1 — the four subtests: **DONE. venustest 91 PASS / 0 FAIL, both arches.**

* All eleven phase-7 subtests PASS on x86_64 and aarch64, zero control-queue
  timeouts in either run. Fresh images, `vfstest` first and once,
  `vfstest` 36/0, `scmtest` 30/0, `drmsmoke` 22/0, `vkrender` 51/0 with
  `s2_checksum = 0x02C0FDC5`, `vktest` 14/0, `vkswap` 21/0 (x86_64).
* The fix is the brief's — drop `RING_IDX` from the real-submit subtests — but
  **the reason recorded for the failure was wrong and is corrected in §2**. Ring
  0 is the CPU ring and needs no creation; Mesa submits `RING_IDX` on ring 0 as
  its *first* execbuffer of every renderer lifetime and it completes. The
  variable is that venustest's 32-byte zero stream is not dispatchable, and a
  fence routed through the renderer context is not retired for a context whose
  dispatch failed.
* **The real Mesa path cannot hit the timeout** (§2.4). No kernel change is
  warranted, exactly as the brief anticipated — but for a different reason than
  the brief carried forward.
* **The guard argument survives intact** (§3). All nine guards rest on "the
  unpatched kernel writes offset 28 on no path", which is flag-independent, and
  the fix keys on `FENCE_FD_OUT` alone. What is lost — the real-submit subtest no
  longer sends Mesa's literal flag word — is named in the report *and* in the
  source, with the compensating control (Mesa's own 16-byte teardown submit,
  covered by `vktest`/`vkrender`/`vkswap` in the same gate). No new guard was
  added, so no second unfailable guard was shipped.
* Landed as `a0325c6` on the box's `main`. Not pushed.

**Next step for the orchestrator:** cherry-pick `a0325c6` (with `e083202` and
`eccc4e9`) onto the Mac tree, and amend TODO item 6 — the "leaked host-side ring"
premise stands, the `RING_IDX` explanation does not. Add the §2.5 robustness note
as its own item if it is wanted; it is not blocking anything.

### Task 2 — `--present`: **RUN, and it reaches the scanout. Pixel capture is impossible on this host.**

* 10/10 `present_*` subtests, `vkrender --present` `rc=0`,
  `--- vkrender done, failures = 0, skipped = 0 ---`. No code was needed; it was
  merely unrun.
* Host-side trace shows the complete handover: `RESOURCE_CREATE_2D 0xb` →
  `RESOURCE_ATTACH_BACKING` → **`SET_SCANOUT id 0, res 0xb, 1920x1080`** →
  `TRANSFER_TO_HOST_2D` → full-frame `RESOURCE_FLUSH`, with the console driver
  reclaiming the scanout when vkrender exits.
* Neither screendump form can photograph it, and §6.1 says exactly why, which
  corrects TODO item 4's account. The remaining half of item 4 is a standalone,
  Vulkan-free dumb-buffer present tool run on the **default** (non-Venus) QEMU
  path, where bare `screendump` does capture the scanout.
