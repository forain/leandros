# PRIME export (TODO item 5) — Mac-checkable verification (2026-08-06)

Lane H, worktree `.claude/worktrees/agent-a859db0b68f28b068`, based on `a0f2c46`.
Patch applied: `prime_handle_to_fd_built_20260806.patch` (+349/−27), clean, no
fuzz, one positional offset only (the recorded `handle_ftruncate` +18).

**Status: IN PROGRESS — this file is written incrementally.**

## 0. Harness, and the positive control run against it

**Which harness produced these numbers.** No new harness was written. Every
number below comes from one of the two pre-existing, in-repo drivers:

* `.claude/skills/run-leandros/driver.py cmd <command> <timeout>` — one command
  per process invocation.
* `scripts/scmrun.py <command> <duration>` — one command per process
  invocation, fixed duration, raw serial dumped verbatim.

**Both are structurally immune to the accumulated-buffer lookback bug** reported
by the other lane, and the reason is worth stating rather than asserting:

* Each is a *separate process* per command. `buf` is a local that starts empty.
  There is no buffer that survives from one command to the next, so a previous
  command's output cannot be re-matched.
* Both **explicitly drain the socket before sending**, so bytes queued from a
  previous command are discarded rather than searched:
  `driver.py:_serial_send` lines 602-609 and `scmrun.py` lines 15-21.
* `scmrun.py` does not `expect()` at all — it reads for a wall-clock duration
  and dumps raw bytes. There is no sentinel to stale-match. Every count below
  was taken by grepping those raw dumps for **literal** `<name>: PASS` /
  `<name>: FAIL` lines emitted by the test binary itself, not by the harness.
* Consequently the harness reports no exit status of its own that a result
  depends on. There is no `rc=0` anywhere in this report.

**Positive control, run before any A/B measurement** (aarch64, patched kernel,
first command of the session):

```
$ driver.py cmd "nosuchbinary_xyz42" 10
error: command not found: nosuchbinary_xyz42
```

The harness reported the known-bad command as failing, and the output contains
nothing from any earlier command. It is not silently returning success.

### A build-environment problem worth recording for every future worktree lane

The first `build-all.sh` in a fresh agent worktree produced an image with **no
`/bin/brush`**, because `build-all.sh` and `mkfs-f2fs-populated.py` resolve the
sibling repos as `$ROOT_DIR/../<repo>` and a worktree's parent is
`.claude/worktrees/`, not `~/code/`. Only `doomgeneric`, `mame` and `relibc`
have symlinks there. The build **exits 0** and prints
`⚠️ brush source not found ... skipping`, so it looks successful; the failure
only shows at runtime as:

```
login: exec failed
session ended, restarting login
```

i.e. `/bin/login` execve()ing a shell that is not in the image. Fixed by adding
`.claude/worktrees/{brush,coreutils,bottom-leandros}` symlinks and rebuilding.
Nothing to do with the patch.

## 1. drmsmoke — the dumb-path gate

**aarch64, patched kernel, fresh image, stock `-device virtio-gpu-pci`:
22 PASS / 0 FAIL. The gate holds, unmoved.**

```
PRIME_HANDLE_TO_FD: PASS
PRIME_MMAP_ALIAS: PASS
PRIME_FD_TO_HANDLE: PASS
```

No subtest moved, so there is no per-subtest movement to explain. The
behavioural change the brief flagged — `vmo_acquire_frames` now returning
ENOMEM instead of growing a borrowed VMO — does not reach drmsmoke, and the
reason is structural rather than lucky: `PRIME_MMAP_ALIAS` maps the exported fd
at **offset 0** for `cd.size` bytes, so `need_pages = ceil(cd.size/4096)`, while
the borrowed page list holds `1 << order` frames from the buddy block that
`cd.size` was rounded **up** into. `need_pages > pages.len()` is therefore never
true on the dumb path. The new guard can only fire on an offset/length that runs
off the end of the allocation, which is exactly the leak it was added to close.

**x86_64, patched kernel, fresh image, stock `-vga none -device virtio-vga`:
22 PASS / 0 FAIL**, same three PRIME lines PASS.

**And the arm that makes this meaningful (measured on aarch64, not x86_64):
drmsmoke on the UNPATCHED kernel is 22 PASS / 0 FAIL too, and `diff` of the two
runs' report lines is EMPTY.**
That is the correct outcome for a non-regression gate, and it is also the
honest reading of what it proves: the patch is byte-identical on the dumb path
by construction (`prime_export_backing` returns `len = (1<<order)*4096` for a
dumb buffer, exactly what `install_dmabuf_vmo` computed before), so **drmsmoke
cannot distinguish patched from unpatched.** Its 22/0 is evidence of no
regression and nothing more. Do not read it as evidence the patch is live.

## 2. venustest A/B — the headline, and it is a negative result

### 2.1 The A/B was run for real, and it discriminates NOTHING on this Mac

Method, exactly as briefed: one image, kernel-only rebuild
(`scripts/m7z2-kernel-only.sh aarch64`, which rebuilds only the Limine kernel
and re-embeds it — it does not touch userland, initrd or the f2fs data image),
so the `venustest` binary was **bit-identical between the two arms** and the
kernel was the only variable.

| Arm | kernel | PASS | FAIL | reports emitted |
|---|---|---|---|---|
| unpatched | `a0f2c46`, three files reverted via `git checkout` | 11 | 31 | 42 |
| patched | same tree + the patch | 11 | 31 | 42 |

```
$ diff <(grep -E ': (PASS|FAIL)$' aa-venustest-UNPATCHED.txt) \
       <(grep -E ': (PASS|FAIL)$' aa-venustest-patched.txt)
$ echo $?
0
```

**The two arms are identical line for line.** Both end
`--- venustest done, failures = 31 ---`.

### 2.2 Why — QEMU on macOS has no blob-capable virtio-gpu device at all

This is not "the HOST3D half is skipped". The **entire** phase-5 block is dead
here, because the guest blob cannot be created either:

```
  param 3D_FEATURES  = 0 (0x0000000000000000)
  param RESOURCE_BLOB = 0 (0x0000000000000000)
  param HOST_VISIBLE  = 0 (0x0000000000000000)
  param CONTEXT_INIT  = 0 (0x0000000000000000)
[GPU] resource_create_blob refused: no RESOURCE_BLOB
```

Phase 5 emits exactly **two** lines on this host, on **both** kernels:

```
phase5_context_init_both: FAIL
phase5_guest_blob_created: FAIL
  (no host3d blob on this host - skipping host-side export checks)
```

`phase5_guest_blob_created` is the `if have {` gate for everything downstream,
so **`phase5_prime_export_guest_blob` is never emitted on either kernel.**

I tried to fix the device rather than accept this, and it is a hard blocker:

```
$ LEANDROS_GPU_DEV="virtio-gpu-pci,blob=on" driver.py start aarch64
qemu-system-aarch64: -device virtio-gpu-pci,blob=on:
    need rutabaga or udmabuf for blob resources
```

`udmabuf` is a Linux kernel facility and rutabaga is not compiled in — the whole
device list on this host is:

```
$ qemu-system-aarch64 -device help | grep -i 'virtio-gpu\|rutabaga'
name "virtio-gpu-device", bus virtio-bus
name "virtio-gpu-pci", bus PCI, alias "virtio-gpu"
```

No `virtio-gpu-gl-pci`, no rutabaga variant, and `blob=on` is refused on the one
device that exists. QEMU 11.0.2, Homebrew, macOS 25.6.0. **There is no QEMU
configuration on this machine that can advertise `VIRTIO_GPU_F_RESOURCE_BLOB`**,
so no blob BO can be created, so no blob can be exported.

### 2.3 This corrects section 2c of the build report

The build lane predicted the Mac would emit **8** phase-5 reports for a
venustest total of **76**, with only the four HOST3D assertions lost. Measured:
**2** phase-5 reports, venustest total **42** (11/31). The guest-blob half is
lost as well, for the same underlying reason the HOST3D half is (no blob
support on this host — not merely no Venus).

The `76`-vs-`80` trap in my brief therefore does not arise, because neither
number is reachable here. **A Mac venustest run contributes zero evidence about
this patch in either direction**, and a reader comparing a Mac run against the
Linux box's 68 baseline should not conclude anything from the difference.

### 2.4 What this does NOT mean

It does not weaken the patch. It means the falsifiability argument for the two
must-fail subtests remains **exactly where the build lane left it: at source
level, unexecuted.** The literal output lines my brief asked for
(`phase5_prime_export_guest_blob: FAIL` on unpatched,
`phase5_prime_export_guest_blob: PASS` on patched) **do not exist in either of
my logs**, and I am not going to paraphrase their absence into a result. The
A/B must be run on the Linux box (`ssh forain@172.16.158.150`,
`virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G -display egl-headless`), where
`RESOURCE_BLOB` is actually advertised.

## 2bis. A real, discriminating A/B IS possible on this Mac — on the dumb path

Rather than stop at "the briefed A/B is unrunnable", I built one that runs here.
**The three guards the patch adds are not blob-specific.** `vmo_acquire_frames`,
`handle_write` and `handle_ftruncate` all key off `vmo.borrowed`, and a **dumb**
buffer's exported fd is a borrowed VMO too — on a path this host fully supports.

I added four throwaway assertions to `drmsmoke`, emitted **after**
`--- drmsmoke done ---` under a `guardprobe_` prefix so the 22-subtest gate
count is untouched (verified: 22 non-probe report lines, 0 non-probe FAILs, on
both kernels). They create a 64x64x32bpp dumb BO (0x4000 = 4 pages, order 2),
export it, and then poke each guard exactly one page past the frame list.

**Same image, kernel-only rebuild, aarch64. Literal output lines:**

UNPATCHED kernel (`a0f2c46`):
```
guardprobe_create_dumb: PASS
guardprobe_export_dumb: PASS
  guardprobe_cap_bytes = 16384
guardprobe_dumb_len_is_buddy_block: PASS
  guardprobe_mmap_ret = 1089753088
guardprobe_mmap_past_frames_refused: FAIL
  guardprobe_write_ret = 8
guardprobe_write_past_frames_refused: FAIL
  guardprobe_ftruncate_ret = 0
guardprobe_ftruncate_refused: FAIL
```

PATCHED kernel:
```
guardprobe_create_dumb: PASS
guardprobe_export_dumb: PASS
  guardprobe_cap_bytes = 16384
guardprobe_dumb_len_is_buddy_block: PASS
  guardprobe_mmap_ret = -1
guardprobe_mmap_past_frames_refused: PASS
  guardprobe_write_ret = -1
guardprobe_write_past_frames_refused: PASS
  guardprobe_ftruncate_ret = -1
guardprobe_ftruncate_refused: PASS
```

Three FAIL → three PASS, with the raw return values printed alongside so the
flip cannot be a report-logic artifact. What this establishes, on hardware
rather than by reading:

1. **The patch is actually live in the running kernel.** This is the one thing
   drmsmoke's 22/0 cannot show, and it is now shown.
2. **All three hazards were real, not theoretical.** Unpatched, the kernel
   returned a *valid mapped address* (`1089753088`) for a mapping one page past
   the frames the DRM layer lent it — i.e. it allocated and leaked an anonymous
   zeroed frame and handed it back as if it were part of the buffer. It also
   accepted an 8-byte `write()` into that region (`8`), and it *succeeded*
   (`0`) at shrinking the borrowed frame list — the order-0-frees-out-of-an-
   order-2-block allocator corruption TODO item 5 names. Patched, all three are
   `-1`.
3. **The dumb path's `len` is byte-identical across the change.**
   `guardprobe_cap_bytes = 16384` = `0x4000` = `(1<<2)*4096` on **both**
   kernels. That is the direct measurement of the patch's "byte-identical to
   today because GBM/EGL fstat it" claim, which the design rests on and which
   nothing else here tests.

**Caveat, stated plainly:** this exercises the three *pre-existing-hazard*
guards. It does **not** exercise `prime_export_backing`'s blob branch, the
`len = b.size` resource-size change, or the empty-page-list HOST3D export. It
is not a substitute for the venustest A/B; it is evidence about a different,
overlapping half of the patch.

The probe was written for this run only, is **not** proposed for landing, and
was reverted from the worktree afterwards. If it is wanted, it is 84 lines in
`userland/drmsmoke/src/main.rs` and it would raise the gate count from 22 to 26.

## 3. What was NOT exercised on this host

Stated explicitly, because missing lines must never be read as passing lines.
**None of the following appears anywhere in any of my logs**, on either kernel:

| Assertion | Status here |
|---|---|
| `phase5_prime_export_guest_blob` | **UNEXERCISED** — never emitted |
| `phase5_prime_export_reports_resource_size` | **UNEXERCISED** — never emitted |
| `phase5_prime_roundtrip_guest_blob` | **UNEXERCISED** — never emitted |
| `phase5_prime_mmap_alias_guest_blob` | **UNEXERCISED** — never emitted |
| `phase5_dmabuf_export_not_truncatable` | **UNEXERCISED** — never emitted |
| `phase5_other_open_export_refused` | **UNEXERCISED** — never emitted |
| `phase5_prime_export_host3d_blob` | **UNEXERCISED** — never emitted |
| `phase5_host3d_export_is_not_mappable` | **UNEXERCISED** — never emitted |
| `phase5_host3d_export_reads_short` | **UNEXERCISED** — never emitted |
| `phase5_host3d_export_refuses_write` | **UNEXERCISED** — never emitted |

`grep phase5_host3d_export_is_not_mappable` over every log in this run returns
nothing. The page-less-export safety argument — the one the whole HOST3D design
turns on — **was not tested at all.**

Also unexercised: `handle_read`'s page-list clamp. My probe does not `read()`
the exported fd, and on a dumb export `pages` is non-empty anyway, so the clamp
is a no-op there. It remains covered only by `phase5_host3d_export_reads_short`
on a Venus host — and its failure mode is a **kernel panic**, not a FAIL line.

Not run, and why: the **x86_64 arm of the guard probe**. The three guards are
architecture-independent VFS code (`servers/vfs/src/lib.rs`), the x86_64
patched kernel passes the stock 22/0 gate, and the probe A/B needs two more
full TCG boots plus a userland rebuild and image regeneration. Flagged as a gap
rather than assumed away.

## 4. Non-regression

All on the **patched** kernel, against **freshly regenerated** images
(`build-all.sh` regenerates `f2fs-data0-<arch>.img` and copies it to `data1`),
with `vfstest` run **exactly once** per image.

| Test | aarch64 | x86_64 | expected | verdict |
|---|---|---|---|---|
| `vfstest` | **36 PASS / 0 FAIL** | **36 PASS / 0 FAIL** | 36/0 | unmoved |
| `scmtest` | **30 PASS / 0 FAIL** | **30 PASS / 0 FAIL** | 30/0 | unmoved |
| `drmsmoke` | **22 PASS / 0 FAIL** | **22 PASS / 0 FAIL** | 22/0 | unmoved |
| `venustest` | 11 PASS / 31 FAIL | 11 PASS / 31 FAIL | — | see §2, host has no blob support; identical on both kernels |

No phantom `chroot_confines_symlink_resolution` / `xattr_list_tmpfs` /
`xattr_list_f2fs` failures — the fresh-image + run-once discipline held. Note
that this retires nothing about the historical aarch64 `xattr_list_f2fs` red:
it passed here, consistent with `project_xattr_list_f2fs_dirty_image`.

`vkrender` (`s2_checksum = 0x02C0FDC5`), also named in item 5's verification
list, was **not** run — it needs the Venus host.

## 5. Verdict

**The Mac-checkable half of TODO item 5 is CLEARED, with one item reclassified
as not Mac-checkable at all.**

* **Priority 1 — the `drmsmoke` 22/0 dumb-path gate: PASSED, both arches.**
  `PRIME_HANDLE_TO_FD`, `PRIME_MMAP_ALIAS` and `PRIME_FD_TO_HANDLE` all PASS.
  Nothing moved, so there is no subtest movement to explain. The
  borrowed-VMO-immutability behavioural change the brief flagged does not reach
  drmsmoke, structurally: it maps the export at offset 0 for `cd.size`, which
  the buddy block was rounded up *from*.

* **Priority 2 — the briefed venustest A/B: NOT RUNNABLE ON THIS MACHINE, and
  this is a hard blocker, not a budget shortfall.** QEMU 11.0.2 on macOS has no
  blob-capable virtio-gpu device: `blob=on` is refused with *"need rutabaga or
  udmabuf for blob resources"*, and neither `virtio-gpu-gl-pci` nor any rutabaga
  variant is compiled in. `VIRTIO_GPU_F_RESOURCE_BLOB` is never advertised, so
  no blob BO exists to export. Both arms were run anyway and are **identical
  line for line (42 reports, 11/31, `diff` empty)**. `phase5_prime_export_guest_blob`
  and `phase5_prime_export_host3d_blob` are **never emitted on either kernel**.
  This must go to the Linux box.

* **Priority 3 — the counts trap: avoided, and the prediction it came from is
  wrong.** The expected-76 figure is unreachable here; the real Mac number is
  **42**. §3 lists all ten phase-5 assertions as UNEXERCISED by name.

* **Priority 4 — non-regression: CLEAN, both arches.** vfstest 36/0, scmtest
  30/0, drmsmoke 22/0.

* **Bonus, and the most load-bearing thing this lane produced: a real
  discriminating A/B does exist on this Mac, on the dumb path, and the patch
  passes it 3 FAIL → 3 PASS with raw return values printed.** That is the only
  on-hardware evidence anywhere that the patch is live in the running kernel
  and that its three guards actually fire — and it independently confirms the
  dumb-path `len` is byte-identical (`0x4000` on both kernels).

**Recommendation.** Item 5 can proceed on the Mac-side evidence: it does not
regress anything, and three of its guards are demonstrated live and load-bearing
on real hardware. It should **not** be recorded as verified until the two
must-fail subtests are A/B'd on the Linux box — the blob branch,
`prime_export_backing`'s resource-size `len`, and the entire HOST3D
empty-page-list safety argument remain source-level claims only.

## 6. Worktree state left behind

`.claude/worktrees/agent-a859db0b68f28b068`, **not committed**, patch applied
and matching the recorded `--numstat` exactly:

```
58   4  drivers/src/drm_device_interface.rs
18   4  kernel/src/syscall.rs
78  19  servers/vfs/src/lib.rs
195  0  userland/venustest/src/main.rs
```

The throwaway `guardprobe_` block in `userland/drmsmoke/src/main.rs` and the
`LEANDROS_GPU_DEV` / `LEANDROS_VGA_DEV` env hooks in
`.claude/skills/run-leandros/driver.py` were reverted.

Three symlinks were added to the **shared** `.claude/worktrees/` directory —
`brush`, `coreutils`, `bottom-leandros` — alongside the pre-existing
`doomgeneric`, `mame`, `relibc`. They are left in place deliberately: without
them every agent worktree builds an image with no shell and boots to
`login: exec failed`.

Raw logs:
`/private/tmp/claude-501/-Users-forain-code-leandros/b625f53e-1f90-454e-8d4a-e4da9aae5da2/scratchpad/`
(`aa-*`, `x86-*`, `build-*`, `kernel-*`).

