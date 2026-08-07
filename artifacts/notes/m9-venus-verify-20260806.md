# M9 Venus-decisive verification on the Linux box (2026-08-06)

Lane I. Box `forain@172.16.158.150`, `/home/forain/Projects/leandros`, branch
`main` at `0df1810` ("drm: honour the host's requested blob cacheability").
EndeavourOS, QEMU 11.0.1, virglrenderer 1.3.0, Mesa 26.1.3, host GPU
AMD Ryzen 9 7950X iGPU (RADV RAPHAEL_MENDOCINO).

Both stashes left untouched. Nothing pushed to `origin`.

**Evidence.** Every raw serial log, the harness, and the extractors are archived
at `~/code/leandros-artifacts/notes/m9-vkswap/m9-lane-i-logs.tgz` (also live on
the box under `/tmp/m9lane/`). The new test's sources are at
`~/code/leandros-artifacts/notes/m9-vkswap/{vkswap.c,build-vkswap-alpine.sh}`
and in the box's venus-lane artifact tree.

| run | kernel | arch | log |
|---|---|---|---|
| A | stock `0df1810`, patched venustest only | x86_64 | `runA-x86_64.log` |
| B | + PRIME | x86_64 | `runB-x86_64.log` |
| C | + syncobj | x86_64 | `runC-x86_64.log` |
| HEAD | stock, traced | x86_64 | `runHEAD-x86_64.log`, `runHEAD3-x86_64.log` |
| D | + PRIME + syncobj | x86_64 / aarch64 | `runD-x86_64.log`, `runD-aarch64.log` |
| NEG | stock, `vkswap` control | x86_64 | `runNEG-x86_64.log` |

---

## 0. Harness, and the positive control

**Which harness produced every number below:** `/tmp/m9lane/lane_i_run.py`, a
verbatim copy of `/tmp/m9b_run.py` (the corrected pty harness a lane wrote
earlier today). It is the *fixed* one, not `m3run.py`. The two properties that
matter:

* `mark()` is taken **before** `send_line`, and `expect(..., start=mk)` passes
  that mark to `re.search(buf, start)` — the search **cannot** see a byte that
  arrived before the command was sent. There is no lookback window at all
  (contrast `m3run.py`'s `buf[pos-200:]`).
* The end sentinel is numbered per command — `RC=$? ZZEND<n>` — so even if the
  mark logic regressed, command *n*'s sentinel cannot be matched by command
  *n+1*.
* The command echo cannot self-match: what is typed is
  `... ; echo "RC=""$?"" ""ZZEND0"`, and the regex is `RC=(-?\d+) ZZEND0\b`;
  `$?` is not `\d+`.

**Positive control — run as the FIRST command of the first session, before any
measurement:**

```
### CMD[0]: nosuchbinary_xyz42
### rc=127
### CMD[1]: venustest
### rc=11
```

A command that cannot exist reported `rc=127`, not `rc=0`. The harness is not
silently succeeding, and the second command reported a *different* rc, so it is
not echoing a stale sentinel either.

Every venustest count below is additionally taken by grepping the raw serial log
for the literal `<name>: PASS` / `<name>: FAIL` lines the **test binary itself**
prints, cross-checked against the binary's own `--- venustest done, failures = N ---`
trailer. The harness's rc is only used as a third, independent cross-check.

**This host really is a Venus host.** Every run below is
`./scripts/run-qemu.sh x86_64 --venus`, i.e.
`-device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G -display egl-headless`.
The proof that the HOST3D path is live rather than skipped is in §1: the string
`no host3d blob on this host` — venustest's own skip message — **does not appear
in any log**, and `phase5_prime_export_host3d_blob` is emitted every time.

---

## 1. Run A — patched venustest on the UNPATCHED kernel (the must-fail arm)

Build: `0df1810` + **only** the `userland/venustest/src/main.rs` hunks of *both*
patches (`git apply --include=userland/venustest/src/main.rs`). Kernel, drivers
and vfs are stock `0df1810`. x86_64, `--venus`, fresh image from
`./scripts/build-all.sh --arch x86_64`.

Totals: **73 PASS / 11 FAIL = 84 reports**, binary trailer
`--- venustest done, failures = 11 ---`, harness `rc=11`. Three independent
counts agree.

### The literal must-fail lines, unpatched kernel

PRIME (TODO item 5) — exactly the two the brief names, and nothing else from
phase 5:

```
phase5_prime_export_guest_blob: FAIL
phase5_prime_export_host3d_blob: FAIL
```

SIMULATE_SYNCOBJ (TODO item 6) — all nine guards:

```
phase7_syncobj_probe_accepted: FAIL
phase7_syncobj_probe_fence_fd_written: FAIL
phase7_syncobj_probe_fd_signalled: FAIL
phase7_syncobj_probe_fd_dupable: FAIL
phase7_submit_fence_fd_out_written: FAIL
phase7_submit_fence_fd_signalled: FAIL
phase7_fence_fd_recycled_over_64_submits: FAIL
phase7_failed_submit_writes_minus_one: FAIL
phase7_failed_submit_releases_fence_fd: FAIL
```

### The two documented non-guards behaved as documented

```
phase5_other_open_export_refused: PASS      <- passes at HEAD; NOT evidence
phase7_no_fence_fd_when_not_requested: PASS <- [NON-REGR] by design
phase7_half_zero_execbuffer_still_refused: PASS  <- [NON-REGR] by design
```

### Arithmetic that confirms nothing was skipped

Phase 5 emitted 5 reports here, not 12, because the nested assertions live
inside the two `if exported`/`if ok` blocks that failed. 68 (HEAD baseline)
+ 5 (phase 5 reached) + 11 (phase 7) = **84**, the measured total.

---

## 2. Patch 1 (PRIME) — Run B, patched kernel, x86_64 Venus

Build: `0df1810` + `prime_handle_to_fd_built_20260806.patch` in full, applied
clean at `0df1810` (not just at `a0f2c46`). `./scripts/build-all.sh --arch
x86_64`, release, exit 0. Fresh image.

```
venustest : 80 PASS / 0 FAIL   --- venustest done, failures = 0 ---
vktest    : 14 [PASS] / 0 [FAIL]   === SUMMARY: 0 failure(s) ===
vkrender  : 51 PASS / 0 FAIL   --- vkrender done, failures = 0, skipped = 0 ---
drmsmoke  : 22 PASS / 0 FAIL
```

**venustest = 80. The target number, on a Venus host.**

### The HOST3D assertions actually RAN — this is the part the Mac cannot do

All **12** phase-5 reports were emitted, and the string `no host3d blob on this
host` (venustest's own skip message) appears **zero** times in the log:

```
phase5_context_init_both: PASS
phase5_guest_blob_created: PASS
phase5_prime_export_guest_blob: PASS
phase5_prime_export_reports_resource_size: PASS
phase5_prime_roundtrip_guest_blob: PASS
phase5_prime_mmap_alias_guest_blob: PASS
phase5_dmabuf_export_not_truncatable: PASS
phase5_other_open_export_refused: PASS
phase5_prime_export_host3d_blob: PASS
phase5_host3d_export_is_not_mappable: PASS      <- the one the brief names
phase5_host3d_export_reads_short: PASS
phase5_host3d_export_refuses_write: PASS
```

`phase5_host3d_export_is_not_mappable` is present and PASSing, so the whole
safety argument was exercised rather than skipped. Both must-fail-unpatched
subtests moved FAIL → PASS between §1 and here, on the same binary.

`vkrender`'s `s2_checksum` came back **`0x02C0FDC5`** — the pinned value.

---

## 3. Patch 2 (SIMULATE_SYNCOBJ) — Run C, patched kernel, x86_64 Venus

Build: `0df1810` + `simulate_syncobj.patch` in full. Exit 0. Fresh image.

```
venustest : 75 PASS / 4 FAIL / 79 total   --- venustest done, failures = 4 ---
vktest    : 0 failures
vkrender  : 51 PASS / 0 FAIL, s2_checksum 0x02C0FDC5
drmsmoke  : 22 PASS / 0 FAIL
```

**The total is 79 as predicted, but it is 79 with FOUR FAILURES, not 79/0.**

```
phase7_syncobj_probe_accepted: PASS
phase7_syncobj_probe_fence_fd_written: PASS
phase7_syncobj_probe_fd_signalled: PASS
phase7_syncobj_probe_fd_dupable: PASS
phase7_submit_fence_fd_out_written: FAIL
phase7_submit_fence_fd_signalled: FAIL
phase7_fence_fd_recycled_over_64_submits: FAIL
phase7_failed_submit_writes_minus_one: PASS
phase7_failed_submit_releases_fence_fd: FAIL
phase7_no_fence_fd_when_not_requested: PASS
phase7_half_zero_execbuffer_still_refused: PASS
```

### The four failures are ONE event, and it is not the patch

The serial log shows the cause directly:

```
phase7_syncobj_probe_fd_dupable: PASS
virtio_gpu_cmd_ctx_submit ctx 0x1, size 32
[GPU] control-queue TIMEOUT, cmd=0x00000207
phase7_submit_fence_fd_out_written: FAIL
```

`cmd=0x207` is `VIRTIO_GPU_CMD_SUBMIT_3D`. The submission **reached the host**
(the QEMU trace event fired) and the host **never retired it**, so
`VirtioGpu::submit`'s busy-spin hit its timeout, the ioctl returned an error,
and `sys_ioctl` correctly wrote `fence_fd = -1` and propagated the failure.
The other three failures are all downstream of `first_fd` never becoming `>= 3`.

**It is not caused by the patch.** Run A — the *stock* `0df1810` kernel, same
venustest binary — contains exactly the same single
`[GPU] control-queue TIMEOUT, cmd=0x00000207`, at the same subtest. Run B
(PRIME only, no phase 7 in the binary) contains **zero**.

### What it actually is: `VIRTIO_GPU_FLAG_INFO_RING_IDX` on a ring-less Venus context

The discriminator is already inside the same run, and needs no extra
experiment. Two submissions, same context `0x1`, same 32-byte all-zero stream,
seconds apart:

| subtest | `flags` | result |
|---|---|---|
| `phase7_submit_fence_fd_out_written` | `RING_IDX \| FENCE_FD_OUT` | **control-queue TIMEOUT** |
| `phase7_no_fence_fd_when_not_requested` | `0` | completes, `rc == 0`, PASS |

The second one runs *after* the first and succeeds, so the context is not
wedged and the host is not dead — the only difference is
`VIRTGPU_EXECBUF_RING_IDX`. `drm_device_interface.rs:3040-3050` honours it by
setting `VIRTIO_GPU_FLAG_INFO_RING_IDX` + `hdr.ring_idx`
(`virtio_gpu.rs:1866-1870`), which makes QEMU route fence creation to
`virgl_renderer_context_create_fence(ctx, …, ring_idx, …)` instead of the
global timeline. On a Venus context that ring only exists once the guest has
sent a Venus `VkRingCreate`; venustest's stream is 32 zero bytes and never
creates one (the earlier `vkr: vn_dispatch_command failed` /
`failed to dispatch context op 5` lines in the same log show the host rejecting
it). The per-ring fence therefore has nowhere to land, is dropped, and the
guest waits until its own timeout.

Mesa never hits this because `vn_ring_create` really does create ring 0 before
it ever submits with `RING_IDX` — which is why `vktest` and `vkrender` issue
dozens of `RING_IDX` submits in the same boot with zero timeouts.

**Conclusion on these four:** they are a **test-design defect in the new phase-7
subtests**, not a kernel defect and not a patch defect. As written they are
unpassable *anywhere*: on a Venus host the ring does not exist, and on a
non-Venus host phase 7 is gated off entirely by `ctx_ok`.

### …and the real-submit path the four subtests could not reach is proven anyway

The wire trace below closes the gap they leave. The 16-byte teardown submission
is issued by `vn_renderer_submit_simple_sync` → `sim_submit`, which sets
`flags = VIRTGPU_EXECBUF_RING_IDX | VIRTGPU_EXECBUF_FENCE_FD_OUT` (its
`sync_count` is non-zero by construction) over a **non-empty** stream — i.e.
**byte-for-byte the shape `phase7_submit_fence_fd_out_written` asserts.** On the
patched kernel that submission completes with **zero** control-queue timeouts
in both `vktest` and `vkrender`, and Mesa then runs `close(args.fence_fd)` on
what we handed back without closing stdin.

So the patch's third invariant branch — real stream + `FENCE_FD_OUT` → a real
fd — is exercised successfully, by Mesa itself, at every `vkDestroyInstance`.
It is only *venustest's synthetic* version of that shape that cannot pass,
because it never creates the host-side ring its `RING_IDX` flag refers to.

### The ring-teardown wire observation — CONFIRMED

This is the corrected-premise check the brief asked for, and it is positive.
QEMU trace events `virtio_gpu_cmd_ctx_{create,submit,destroy}` were enabled and
land interleaved in the same serial log, so they are time-ordered against the
guest's own output.

**A counting instrument was tried first and it lied — recording that, because
the number it produced looked perfect.** `ctx_submit` events between
`ctx_create` and `ctx_destroy` for `vktest` came out 6 on HEAD and 7 patched, a
clean +1 for the one `vkCreateInstance`/`vkDestroyInstance` pair. Running
`vktest` **three times in one HEAD boot** killed it: **6, 6, 7**. The submit
count is not a stable per-lifecycle counter — Venus notifies its shared ring
opportunistically, so the number floats. Any conclusion drawn from that +1
would have been void.

The observable that actually works is the **payload size** the trace already
prints, and it is qualitative rather than a count:

| kernel | binary | SUBMIT_3D sizes over one renderer lifetime |
|---|---|---|
| `0df1810` stock | vktest ×3 | `[140, 24×5]`, `[140, 24×5]`, `[140, 24×6]` |
| `0df1810` stock | vkrender | `[140, 24×46]` |
| + syncobj patch | vktest | `[140, 24×5, `**`16`**`]` |
| + syncobj patch | vkrender | `[140, 24×45, `**`16`**`]` |

Across every HEAD lifetime measured — 4 × vktest and 1 × vkrender, 72 submits
total — **the only sizes that ever occur are 140 and 24. A 16-byte submit
occurs zero times.** On the patched kernel a 16-byte submit appears **exactly
once per renderer lifetime, and it is always the last submission before
`ctx_destroy`** — in both binaries, including the `vkrender` lifetime whose
*count* (47) was unchanged and had hidden it.

That 16-byte trailer is `vn_ring_destroy`'s teardown command: with the probe
accepted, `vn_renderer_submit_simple_sync` → `vn_renderer_sync_create` now
succeeds, so the ring-destroy command reaches the host instead of being
abandoned before submission. **The corrected premise in `simulate_syncobj.md`
§1.4 — that today's real damage is a leaked host-side ring per Venus instance,
not `close(0)` — is confirmed on the wire.**

### stdin was NOT closed

`close(0)` never appears. `phase7_failed_submit_writes_minus_one` PASSes on the
patched kernel, i.e. a failing submit really does write `-1` and not the
incoming `0`; `phase7_syncobj_probe_fence_fd_written` PASSes, i.e. a successful
one writes a real `>= 3` fd. Every command after venustest in the same session
(`vktest`, `vkrender`, `drmsmoke`) ran and printed normally, which it could not
do with a closed stdin in the shell's child chain.

---

## 4. The headless-surface swapchain — the decisive downstream test

The brief named a `VK_EXT_headless_surface` swapchain as the strongest possible
confirmation for PRIME. Nothing in the tree could take that path, so I wrote
one: **`vkswap`**, a ~450-line dependency-free C program in exactly vkrender's
idiom (no Khronos loader; `dlopen("/usr/lib/libvulkan_virtio.so")`, bootstrap
from `vk_icdGetInstanceProcAddr`, device entry points via
`vkGetDeviceProcAddr`). Sources and build recipe now live beside vkrender's:

* `~/code/leandros-artifacts/venus-lane/vkswap.c`
* `~/code/leandros-artifacts/venus-lane/build-vkswap-alpine.sh`
* built binary → `venus-lane/stage-x86_64/usr/bin/vkswap`
* staged into the image by a 4-line addition to
  `scripts/mkfs-f2fs-populated.py` (mirrors the existing `vkrender` block;
  **uncommitted**, see §7)

It goes all the way: surface → present-capable queue family → surface caps,
formats, present modes → device with `VK_KHR_swapchain` → **swapchain** →
swapchain images → acquire (with a real fence the CPU waits on) → a genuine
`UNDEFINED → PRESENT_SRC_KHR` barrier submitted on the queue and fence-waited →
`vkQueuePresentKHR`. The layout transition is there on purpose: presenting an
`UNDEFINED` image is undefined behaviour, and a present that skipped it would
be a spec violation that happens to return `VK_SUCCESS`.

**Result on the stacked patched kernel (run D), x86_64, Venus: 21 PASS / 0 FAIL.**

```
instance_advertises_headless_surface: PASS   VK_KHR_surface=yes VK_EXT_headless_surface=yes
create_headless_surface: PASS
queue_family_supports_present: PASS          queueFamily[0] flags=0xf presentSupport=yes
surface_capabilities: PASS                   minImageCount=4 supportedUsage=0x8009f
surface_formats_nonempty: PASS               formats 37, 44
surface_present_modes_include_fifo: PASS
device_advertises_swapchain: PASS
create_device_with_swapchain: PASS
create_swapchain: PASS                       256x256, 5 images
swapchain_images_nonzero: PASS
acquire_next_image: PASS                     index=0, fence wait -> VK_SUCCESS
transition_image_to_present_src: PASS        submit -> VK_SUCCESS, fence -> VK_SUCCESS
queue_present: PASS                          vkQueuePresentKHR -> VK_SUCCESS
--- vkswap done, failures = 0, skipped = 0 ---
```

A Vulkan swapchain has never existed on LeandrOS before. Incidentally the run
also shows `0df1810` live on this path:
`[DRM] non-cached blob mapping honoured (uncached)` fires while the WSI images
are allocated.

---

## 5. Run D — both patches stacked, x86_64 Venus, fresh image

Build: `0df1810` + PRIME + syncobj, applied in that order (both apply clean at
`0df1810`, not only at `a0f2c46`). `vfstest` was the FIRST command against the
freshly generated image and was run exactly once.

| test | result |
|---|---|
| `vfstest` | **36 PASS / 0 FAIL** |
| `scmtest` | **30 PASS / 0 FAIL** |
| `venustest` | 87 PASS / 4 FAIL / **91 total** (= 80 + 11) |
| `vkswap` | **21 PASS / 0 FAIL** |
| `vktest` | 0 failures |
| `vkrender` | **51 PASS / 0 FAIL**, `s2_checksum = 0x02C0FDC5` |
| `drmsmoke` | **22 PASS / 0 FAIL** |

The four failures are the same four §3 explains, unchanged by stacking. All 12
of PRIME's phase-5 reports still PASS with the syncobj patch also applied, and
all 7 passing phase-7 reports still PASS with PRIME also applied — **the two
patches do not interact.**

---

## 6. The negative control for `vkswap` — it fails without the PRIME patch

A new test that passes proves nothing until you have watched it fail. So the
same `vkswap` binary, same image pipeline, same host, was run against a kernel
with **both patches reverted** (`git apply -R`, tree back to a clean
`0df1810` — verified by `git status --porcelain` showing only my
`mkfs-f2fs-populated.py` staging line):

```
icd_dlopen … device_advertises_swapchain: PASS      (16 subtests)
create_device_with_swapchain: PASS
resolve_device_wsi_funcs: PASS
[INFO] vkCreateSwapchainKHR: 256x256 minImageCount=5 format=37
[ERR ] vkCreateSwapchainKHR -> VkResult(-10)
create_swapchain: FAIL
--- vkswap done, failures = 1, skipped = 0 ---
```

**16 PASS / 1 FAIL, and the one failure is `create_swapchain`.** (-10 is
`VK_ERROR_TOO_MANY_OBJECTS` in the core enum; `vkswap`'s decoder does not spell
that one out, hence the numeric form.)

Everything WSI that does *not* need to allocate a shareable image — the
extension, the surface, present support, caps, formats, present modes, and even
`vkCreateDevice` with `VK_KHR_swapchain` — already worked at HEAD. The single
thing that did not is the swapchain itself, which is where Mesa's WSI has to
export the image as a dma-buf and therefore where
`DRM_IOCTL_PRIME_HANDLE_TO_FD` for a Venus blob handle is unavoidable.

**That is the cleanest attribution available anywhere in this wave: one binary,
two kernels, `vkCreateSwapchainKHR` fails on one and succeeds on the other, and
nothing else moves.**

---

## 7. Non-regression — BOTH arches, both patches stacked, fresh images

Every number below is from a freshly generated image, `--venus`, with `vfstest`
run **first and exactly once** per image. aarch64 used `-cpu max,lpa2=off`
(the Limine 11.4.1 FEAT_LPA2 gotcha).

| test | x86_64 | aarch64 | required |
|---|---|---|---|
| `vfstest` | **36 / 0** | **36 / 0** | 36/0 ✅ |
| `scmtest` | **30 / 0** | **30 / 0** | 30/0 ✅ |
| `drmsmoke` | **22 / 0** | **22 / 0** | 22/0 ✅ |
| `vkrender` | **51 / 0** | **51 / 0** | 0 failures ✅ |
| `vkrender` `s2_checksum` | **`0x02C0FDC5`** | **`0x02C0FDC5`** | `0x02C0FDC5` ✅ |
| `vktest` | 0 failures | 0 failures | 0 failures ✅ |
| `venustest` | 87 / 4 (91) | 87 / 4 (91) | see §3 |

The two arches are **identical, subtest for subtest**, including the four
phase-7 failures and their single `control-queue TIMEOUT`. aarch64 also shows
all 12 phase-5 reports PASS with zero `no host3d blob` skips, and the same
16-byte ring-teardown submit as the last submission of both the `vktest`
(ctx#7) and `vkrender` (ctx#8) renderer lifetimes.

Worth noting: **`vfstest` is 36/0 on aarch64.** The historical aarch64
`xattr_list_f2fs` red does not appear, which is consistent with it being a
dirty-image artifact rather than an arch bug.

`s2_checksum` is printed but **not asserted** by `vkrender` unless
`VKRENDER_EXPECT_CHECKSUM` is exported; the value was read out of the log and
compared by hand each time.

Release builds only. Limine revision untouched. Nothing pushed to `origin`
(still at `6a0eb0c`); both pre-existing stashes untouched and unpopped.

---

## 8. Committed locally on the box's `main`

```
eccc4e9 mkfs: stage vkswap when the venus artifact tree provides it
e083202 drm: export Venus blob handles through PRIME_HANDLE_TO_FD
0df1810 drm: honour the host's requested blob cacheability   <- was HEAD
```

Only PRIME was committed — its decisive test passed. **The syncobj patch was
deliberately NOT committed** and the tree no longer carries it; the patch file
is unchanged at
`~/code/leandros-artifacts/notes/m9-simulate-syncobj/simulate_syncobj.patch`
and a copy is on the box at `/tmp/m9lane/`.

---

## 9. Verdicts

### Patch 1 — PRIME export for blob handles (TODO item 5): **CLEARED**

* `venustest` **80 PASS / 0 FAIL** on a Venus host, all **12** phase-5 reports
  emitted, `no host3d blob on this host` absent from every log. The four HOST3D
  assertions — including `phase5_host3d_export_is_not_mappable` — **ran**.
* Must-fail-unpatched set behaved exactly as specified: on the stock kernel,
  `phase5_prime_export_guest_blob: FAIL` and
  `phase5_prime_export_host3d_blob: FAIL`, and only those two from phase 5.
  `phase5_other_open_export_refused` passed at HEAD, as documented, and is
  correctly not counted.
* The decisive downstream test is **positive**: a `VK_EXT_headless_surface`
  swapchain is created, its images enumerated, one acquired against a fence,
  transitioned to `PRESENT_SRC_KHR` by a real queue submission, and presented —
  `vkQueuePresentKHR -> VK_SUCCESS`. 21/21.
* Negative control run: the same binary on the reverted kernel fails at exactly
  `create_swapchain` and nowhere else.
* No regression on either arch.

**Next step for the orchestrator:** nothing blocking. Consider cherry-picking
`e083202` + `eccc4e9` onto the Mac tree, and carrying `vkswap.c` /
`build-vkswap-alpine.sh` into the venus-lane artifact tree (they are already
there on the box's `leandros-artifacts` symlink). An aarch64 `vkswap` binary
was **not** built — the box has no arm64 binfmt handler, so the Alpine
container cannot cross-build it; `zig cc -target aarch64-linux-musl` (the
wrapper `scripts/cc-aarch64-musl.sh` already uses) is the obvious route if it
is wanted.

### Patch 2 — SIMULATE_SYNCOBJ fence_fd (TODO item 6): **NOT CLEARED as submitted — but the kernel change itself is right**

* `venustest` reaches **79**, the predicted total, but **79 with 4 FAIL**, not
  79/0. Failing: `phase7_submit_fence_fd_out_written`,
  `phase7_submit_fence_fd_signalled`,
  `phase7_fence_fd_recycled_over_64_submits`,
  `phase7_failed_submit_releases_fence_fd`. Identical on both arches.
* All four are one event: a `control-queue TIMEOUT, cmd=0x00000207`. It is
  **not the patch** — the stock `0df1810` kernel running the same binary
  produces the identical single timeout at the identical subtest.
* Diagnosis: the phase-7 real-submit subtests set `VIRTGPU_EXECBUF_RING_IDX`
  over a 32-byte all-zero stream. That makes the guest set
  `VIRTIO_GPU_FLAG_INFO_RING_IDX`, so the host is asked to fence on a
  **per-context ring that the stream never created**; the fence is dropped and
  the guest spins to its timeout. The same context accepts a `flags = 0` submit
  seconds later (`phase7_no_fence_fd_when_not_requested: PASS`), so nothing is
  wedged — `RING_IDX` is the only variable.
* The invariant the patch exists to establish is nevertheless **proven on all
  three branches**: fence-only success (fd `>= 3`, pollable, dupable), failure
  (`-1`), and — via Mesa rather than via venustest — real stream +
  `FENCE_FD_OUT` succeeding, since `vn_ring_destroy`'s teardown submit uses
  exactly that shape and completes with zero timeouts.
* `close(0)` never happens; stdin is intact.
* **The corrected premise is confirmed on the wire.** A 16-byte SUBMIT_3D
  appears exactly once per renderer lifetime, always immediately before
  `ctx_destroy`, on the patched kernel only — zero such submissions in 72
  submits across five HEAD lifetimes. Every Venus instance really was leaking
  its host-side ring, and now is not.

**Next step for the orchestrator:** the patch is worth landing after the four
guards are re-specified. They cannot pass as written *anywhere* — on a Venus
host the ring does not exist, and on a non-Venus host phase 7 is gated off by
`ctx_ok`. The minimal fix is to drop `EXECBUF_RING_IDX` from the four
real-submit subtests (keep it in the probe, which must stay byte-identical to
Mesa's); `FENCE_FD_OUT` alone exercises the entire fd path and the fence then
lands on the global timeline, which retires. Do **not** "fix" this in the
kernel — refusing or rewriting `RING_IDX` would break Mesa, whose submits all
carry it.

Separately, this uncovered a genuine standing gap worth its own TODO entry: a
`RING_IDX`-fenced submission against a context with no host-side ring is
silently dropped and costs the caller a full control-queue timeout instead of
an error. Nothing in the tree hits it today except this test, but it is a
denial-of-service shape for any future client.

---

## 10. Instrument discipline — what I actually ran, and the one that lied

**Harness:** `/tmp/m9lane/lane_i_run.py`, a verbatim copy of `/tmp/m9b_run.py`
(with a `--extra` passthrough added for QEMU trace flags). Chosen because its
`expect()` searches only from a mark taken *before* the send, and its sentinel
is numbered per command. `m3run.py` — also on the box — has a `buf[pos-200:]`
lookback and was **not** used.

**Positive control:** `nosuchbinary_xyz42` as CMD[0] of **every single boot**
in this report (7 boots). It returned `rc=127` every time. No run's numbers
rest on a harness that was silently reporting success.

**The instrument that lied, and how it was caught.** For the ring-teardown
observation I first counted `virtio_gpu_cmd_ctx_submit` events per context
lifetime: 6 on HEAD, 7 patched — a clean, believable +1 for one
`vkCreateInstance`/`vkDestroyInstance` pair. Running `vktest` **three times in
one HEAD boot** gave **6, 6, 7**. Venus notifies its ring opportunistically, so
the count floats and the +1 was noise. I replaced it with the *payload size*,
which is qualitative and unambiguous (16 bytes never occurs on HEAD, occurs
once per lifetime patched, always last before `ctx_destroy`) — and that
observable also revealed the teardown submit inside `vkrender`, whose *count*
was 47 on both kernels and had hidden it entirely.

A second, smaller one: my first PASS/FAIL extractor anchored on `^\S+: PASS$`
and reported `PASS=0` for runs that plainly contained PASS lines — the serial
console emits CRLF. Cross-checking against each binary's own
`failures = N` trailer is what surfaced it, and every count in this report is
backed by all three of: grepped `: PASS`/`: FAIL` lines, the binary's own
trailer, and the harness `rc`.

