# PRIME export for blob handles — first build report (2026-08-06)

Lane C, worktree `.claude/worktrees/agent-a77783519b5853e03`, based on `a0f2c46`.
No QEMU was run (another lane owns it). Everything below is a build result or a
source-level argument.

Rebuilt patch: `prime_handle_to_fd_built_20260806.patch` (alongside this file).

---

## 1. Apply and compile — the headline

**The patch builds clean on both architectures on its first compile. There was no
compile fallout at all.**

### Apply

`git apply` against `a0f2c46` succeeded with **no fuzz and no rejects**. One
positional offset only:

```
Checking patch servers/vfs/src/lib.rs...
Hunk #5 succeeded at 5107 (offset 18 lines).
```

That is hunk 5 (the `handle_ftruncate` borrowed-VMO guard); the patch was
prepared at `9d27ae0` and `a0f2c46` has 18 lines of unrelated growth above
`handle_ftruncate`. Context matched exactly, so the offset is bookkeeping, not
fuzz. The other three files applied at their recorded offsets.

### Build

Both via the project's own script, release only:

| Arch | Command | Exit | `error:` count |
|---|---|---|---|
| aarch64 | `./scripts/build-all.sh --arch aarch64` | 0 | 0 |
| x86_64 | `./scripts/build-all.sh --arch x86_64` | 0 | 0 |

Both runs went all the way through kernel, direct-boot kernel, userland, initrd
and the populated F2FS image (`🎉 Build Complete!`).

**Compile fallout: none.** No errors, and no *new* warnings either — grepping
both build logs for the line ranges the patch touches
(`drm_device_interface.rs:1210-1260`, `syscall.rs:6040-6100`,
`vfs/src/lib.rs:548-610` and `:667-685`, all of `venustest/src/main.rs`'s new
block) returns nothing. Nothing was changed to make it compile.

Two things worth recording because they were the plausible failure modes and
neither fired:

* `install_dmabuf_vmo` has exactly one caller in the tree
  (`kernel/src/syscall.rs:6098`), which the patch updates in the same hunk — the
  signature change from 5 to 6 parameters breaks nothing else.
* `venustest`'s new `lseek` extern resolved at link time against the vendored
  relibc. That is real evidence, not an assumption: `venustest` is in
  `RELIBC_LINKED` in `scripts/build-userland.sh` and both images packed it.

One pre-existing warning sits close enough to the patch to be worth
disclaiming: `servers/vfs/src/lib.rs:642: variable does not need to be mutable`
(`let mut tmp = TMP_FILES.lock()` in `vmo_acquire_frames`). It is on an
untouched line — the patch's hunk starts 25 lines below it — and it is present
at HEAD.

### Formatting and lints — what "as the repo does" actually is

The repo has **no** `rustfmt.toml`, no `clippy.toml`, no CI workflow, and is not
rustfmt-formatted: `cargo fmt --check -p vfs-server` on a **stashed, pristine
`a0f2c46`** emits **7159 lines** of diff. Running `cargo fmt` would therefore be
a tree-wide reformat, not a check, and would destroy the hand-aligned comment
blocks the patch deliberately preserves. The meaningful check is the one the
patch author used — that every touched file *parses* under rustfmt — and all
four do:

```
rustfmt --edition 2021 --check --emit stdout <file>   # no parse errors, 4/4
```

Clippy is likewise not a gate here: on pristine `a0f2c46` it **fails to
compile** with 2 hard `clippy::not_unsafe_ptr_arg_deref` errors in `sched`
(`sched/src/lib.rs:514,527`) plus several hundred warnings across `mm`, `sched`,
`vfs-server` and `drivers`. Suppressing those two errors to get a reading on the
touched crates, clippy reports **zero diagnostics inside any line range the
patch adds or edits**. (The `doc list item overindented` hits in
`drm_device_interface.rs:843-848` and `:986-990` are on the untouched
`win_off`/`map_phys` and `CTX_BIND_NO_SLOT` docs.)

All doc comments were preserved verbatim; nothing was reflowed.

---

## 2. Per-subtest falsifiability

No QEMU was available, so this is a source-level argument traced end to end
through the actual code paths, not a run.

Phase 5 emits **12 reports** on a full pass (9 as originally written, plus the
three added in the second pass — see 2b). Verdict summary:

| # | Subtest | Can fail? | Fails at unpatched HEAD? |
|---|---|---|---|
| 0 | `phase5_open_two_fds` | n/a — hard-coded `false` | not emitted on a healthy run |
| 1 | `phase5_context_init_both` | yes (weak: gates nothing) | no |
| 2 | `phase5_guest_blob_created` | yes | no |
| 3 | `phase5_prime_export_guest_blob` | **yes** | **yes — deterministically** |
| 4 | `phase5_prime_export_reports_resource_size` | **yes** | not emitted (nested) |
| 5 | `phase5_prime_roundtrip_guest_blob` | yes | not emitted (nested) |
| 6 | `phase5_prime_mmap_alias_guest_blob` | **yes — strongest** | not emitted (nested) |
| 6b | `phase5_dmabuf_export_not_truncatable` † | **yes** | not emitted (nested) |
| 7 | `phase5_other_open_export_refused` | yes, vs. its own mutation | **no — passes at HEAD too** |
| 8 | `phase5_prime_export_host3d_blob` | **yes** | **yes — deterministically** |
| 9 | `phase5_host3d_export_is_not_mappable` | **yes** | not emitted (nested) |
| 9b | `phase5_host3d_export_reads_short` † | **yes — as a kernel panic** | not emitted (nested) |
| 9c | `phase5_host3d_export_refuses_write` † | **yes** | not emitted (nested) |

† added in the second pass; falsifiability traced in 2b.

**Nothing is vacuous.** Every report either fails closed or has a traced
mutation that flips it. But three things need flagging, below.

### The three anchors

**Anchor 1 — both export subtests must fail unpatched. Confirmed for #3 and #8.**
`blob.bo_handle` comes from `NEXT_BLOB_HANDLE`, a `static AtomicU32` starting at
`0x4000` and only incrementing. At HEAD the `PRIME_HANDLE_TO_FD` arm calls
`dumb_buffer_phys_order(handle)`, whose entire body is
`DUMB_BUFFERS.lock().get(&handle)`. `DUMB_BUFFERS` is keyed by
`Driver::next_handle()` — `static mut NEXT_HANDLE: u32 = 1`, +1 per dumb-buffer
creation, never recycled — so on a fresh boot running venustest the dumb handles
are in the low tens and never reach `0x4000`. The lookup misses, the arm returns
`-22`, `exported == false`, **FAIL**. Both #3 and #8 take this identical path.
With the patch, `prime_export_backing` consults `blob_lookup` first and returns
`Some`. This is exactly the "HEAD is the backed-out state" property required.

*Caveat worth recording (pre-existing, not introduced here):* the two handle
spaces are disjoint only for the first 16383 dumb-buffer creations of a boot.
`prime_export_backing` tries blob-first, so past a collision a colliding dumb
handle would resolve to the blob. The same shape already exists in `bo_exists`,
`bo_fence` and `virtgpu_handle_map`. It does not affect venustest, which runs
early.

**Anchor 2 — reverting the `len` change must report 0x4000 instead of 0x3000.
Confirmed exactly.** `P5_BLOB_SIZE = 0x3000` (12288). `order_for_bytes(0x3000)`:
`pages = (12288+4095)>>12 = 3`; `pages != 1`, so
`order = 64 − (3−1).leading_zeros() = 64 − 62 = 2` → buddy block = 4 pages =
**0x4000**. The blob is deliberately non-power-of-two for precisely this reason.

- *With* the patch: `prime_export_backing` returns `len = b.size = 0x3000`;
  `install_dmabuf_vmo` sets `tmp[idx].len = len`; `handle_lseek`'s `TmpFile` arm
  (`servers/vfs/src/lib.rs:3574`) computes `SEEK_END` from `tmp[idx].len` →
  **0x3000** → PASS.
- *Reverting* it (restoring `tmp[idx].len = capacity`, `capacity =
  (1<<order)*4096`) → **0x4000** → FAIL, with the diagnostic printed.

The `lseek` extern the subtest needs resolved at link time against the vendored
relibc — real evidence, since both images packed `venustest`.

*Cosmetic flag, left unchanged:* the diagnostic uses `out_u64`, which prints
**decimal**, so the line reads `expected size 12288, got 16384`, not
`0x3000`/`0x4000`. Unambiguous, just not hex. Changing it is not compile fallout,
so I did not touch it.

**Anchor 3 — `phase5_host3d_export_is_not_mappable` must be demonstrably
failable with its guard line deleted. Confirmed, and the guard line is the only
thing between the two outcomes.** Traced:

1. A pure `BLOB_MEM_HOST3D` blob is not `guest_backed`
   (`drm_device_interface.rs:3397-3408`), so `phys = 0` and `BlobBuf.phys = 0`.
2. `prime_export_backing` returns `phys: 0`; `install_dmabuf_vmo`'s `if phys == 0`
   arm installs an **empty** `pages` vec with `len = 0x1000`, `borrowed: true`.
3. `mmap(NULL, 0x1000, RW, MAP_SHARED, ph.fd, 0)` reaches `sys_mmap`'s
   `MAP_SHARED` + `TmpFile` branch (`kernel/src/syscall.rs:1664`). **There is no
   fallback** — both arms of that `match` `return`, so nothing else can satisfy
   the mapping. It calls `vmo_acquire_frames(pid, fd, 0, 0x1000)` →
   `need_pages = 1`.
4. **With** the guard `if vmo.borrowed && need_pages > vmo.pages.len() { return
   None; }`: `1 > 0` → `None` → `sys_mmap` returns `-12`. Userspace sees
   `p as isize == −12 <= 0` → **PASS**.
5. **Without** the guard: control falls into
   `while vmo.pages.len() < need_pages { vmo_alloc_zeroed_frame() }`, allocates
   one zeroed anonymous frame, returns `Some(frames)`, `map_shared_frames`
   succeeds, `sys_mmap` returns `virt as isize` — a positive address →
   `p as isize > 0` → **FAIL**.

That also confirms the hazard is real rather than theoretical: without the guard,
Mesa gets a mapping of zeroed anonymous RAM standing in for a host resource.

### The strongest assertion, and why it cannot pass spuriously

**#6 `phase5_prime_mmap_alias_guest_blob`** is the only subtest that can tell a
real export from an fd over unrelated memory. It mmaps the dmabuf fd
(`MAP_SHARED` on a `TmpFile` → `vmo_acquire_frames` → the borrowed frames, which
*are* the blob's buddy pages) and separately mmaps `fd_a` at the `VIRTGPU_MAP`
token (→ `phys_addr != 0` → `map_device` of the same physical base), writes
`0x5EED1234` through the first and reads it back through the second.

- It **fails closed**: `alias_ok` starts `false` and stays false if `VIRTGPU_MAP`
  or either mmap fails. No failure mode silently passes.
- The two mmaps are distinct calls returning distinct VAs, so there is no trivial
  same-address pass.
- Bounds: `vmo_acquire_frames(off=0, len=0x3000)` → `need_pages = 3`; the
  borrowed page list has `1<<order = 4` entries, because the patch keeps building
  `1<<order` frames while setting `len` to the resource size. `3 > 4` is false,
  so the new immutability guard does not trip. This is the doc comment's "`len`
  never exceeds the frames listed" clause, and it holds.

### Three flags

**Flag A — #7 `phase5_other_open_export_refused` passes at unpatched HEAD.**
Not vacuous: `device_open_alloc()` returns `slot + 1` so a successful open never
has `open_id == 0`, and `fd_a` still holds its slot when `fd_b` opens, so the two
ids are distinct and non-zero; `open_may_reach(caller, owner) = caller == 0 ||
owner == 0 || caller == owner` → false → `blob_lookup` → `None` → falls through
to `dumb_buffer_phys_order` → miss → `-22`. Replacing `blob_lookup(handle,
open_id)` with a bare `BLOB_BUFFERS.lock().get(&handle)` makes fd_b's export
succeed → FAIL. So it *is* falsifiable against the mutation it guards. **But it
also passes at HEAD, where everything is refused.** It is a non-regression guard
on the scoping rule, not evidence the patch is live. Do not count it among the
"must fail unpatched" pair.

**Flag B — on a non-Venus host, #8 and #9 vanish without a FAIL.** The whole
HOST3D block is guarded by `if hrc == 0 && hblob.bo_handle != 0`; otherwise it
prints `(no host3d blob on this host - skipping ...)` and emits **no reports**.
On the Mac (plain `virtio-gpu-pci`) venustest will report **75, not 77**, and
*both* HOST3D assertions — including the one carrying the entire safety argument
— disappear silently. **If those two lines are missing from a run log, the safety
argument was not tested. That must not be read as "passed".** TODO item 5's
`68 → 77` figure is only valid on the Linux/Venus box.

**Flag C — three hunks have no subtest at all.** These are item 7's fixes riding
along, and they land uncovered:

- `handle_read`'s `n.min(vmo.pages.len()*4096 − cur)` clamp. Nothing in phase 5
  `read()`s the exported fd. This is **not** defensive polish: without it, a
  `read()` of a HOST3D token fd runs `vmo_copy_out` over an empty page list and
  **panics in the kernel**. It is the most consequential untested line in the
  patch. One line in #9's block would cover it: `read(ph.fd, &mut buf, 8) == 0`.
- `handle_write`'s `if !vmo.borrowed` growth guard. Untested.
- `handle_ftruncate`'s `if vmo.borrowed { return err_reply(-1) }`. Untested —
  and this is the one closing the allocator-corruption hazard (order-0
  `unref_or_free` out of an order-N buddy block). Worth an
  `ftruncate(ph.fd, 0x1000) != 0` assertion.

**Flag C is now CLOSED** — all three assertions were added in a second pass and
both arches rebuilt clean. See 2b for each one's falsifiability mutation. The
paragraph above is retained as the rationale for why they were needed.

### 2b. Flag C CLOSED — three assertions added (second pass)

The three uncovered hunks now have subtests. Each was placed where the hazard it
guards actually bites, and each has a traced mutation that flips it.

| Subtest | Guards | Mutation that makes it FAIL |
|---|---|---|
| `phase5_dmabuf_export_not_truncatable` | `handle_ftruncate`'s `if vmo.borrowed` | delete that line → returns 0 instead of −1 |
| `phase5_host3d_export_reads_short` | `handle_read`'s page-list clamp | delete the clamp → **kernel panic** |
| `phase5_host3d_export_refuses_write` | `handle_write`'s `if !vmo.borrowed` | delete that guard → returns 8 instead of −1 |

Two externs were added to venustest (`read`, `ftruncate`); both resolve against
vendored relibc — confirmed by both images linking and packing `venustest`.

**`phase5_dmabuf_export_not_truncatable`** — placed on the **guest** blob's
exported fd, because that is where the hazard is real: that fd's frame list IS
the DRM layer's order-2 buddy block. `ftruncate(ph.fd, 0x1000)` shrinks
0x3000 → 0x1000.
- *With* the guard: `handle_ftruncate`'s `TmpFile` arm hits
  `if vmo.borrowed { return err_reply(-1) }` → EPERM → non-zero → PASS.
- *Without* it: `new_pages = 1 < old_pages = 4`, so the `else` branch runs
  `for p in 1..4 { mm::pageref::unref_or_free(vmo.pages[p], 0) }` — three
  **order-0 frees out of a single order-2 allocation** — then `ok_reply()` → 0 →
  FAIL. The mutation does not merely leak; it corrupts the buddy allocator,
  which is exactly the hazard TODO item 7 names.
- Deliberately not placed on the HOST3D fd, where `pages` is empty and the
  shrink loop iterates zero times: the return value would still flip, but the
  corruption would go unexercised. Shrink on the guest fd tests both.

**`phase5_host3d_export_reads_short`** — `read(ph.fd, buf, 8)` on the token fd
(`len = 0x1000`, `pages` empty, opened O_RDWR by the export path).
- *With* the clamp: `remaining = 0x1000`, `n = 8.min(0x1000).min(4096) = 8`,
  then `n = n.min((0 * 4096).saturating_sub(0)) = 0` → `val_reply(0)` → read
  returns 0 → PASS. Zero is the correct answer: there are no bytes here, only a
  handle.
- *Without* it: `n` stays 8 and `vmo_copy_out(vmo, 0, buf, 8)` evaluates
  `vmo.pages[0]` (`servers/vfs/src/lib.rs:404`) on an **empty `Vec`**.

> **⚠ READ BEFORE RUNNING.** The failing form of this assertion is an
> out-of-bounds index **panic in kernel context**, not a `FAIL` line. If the read
> clamp is removed or reverted, the subtest does not print `FAIL` — the kernel
> panics inside `handle_read` and the run dies there. That is still a useful
> test, since the hazard is otherwise completely silent, but a panic at this
> point must **not** be triaged as an unrelated regression: it is this assertion
> firing. It is also why the clamp is not optional polish.

**`phase5_host3d_export_refuses_write`** — `write(ph.fd, buf, 8)` on the same fd.
- *With* the guard: `if !vmo.borrowed` skips the growth loop, so
  `cap_bytes = 0`, `n = 8.min(0.saturating_sub(0)) = 0`, and
  `if n == 0 { return err_reply(-28) }` → ENOSPC → negative → PASS.
- *Without* it: the `while` loop appends one zeroed anonymous frame →
  `cap_bytes = 4096` → `n = 8` → `val_reply(8)` → FAIL. The mutation both leaks
  that frame permanently (`vmo_free_slot` returns early for `borrowed`) and
  claims to have stored 8 bytes into a host resource it never touched.

All three fail closed; none is vacuous. Like #4/#5/#6 and #9, the two HOST3D
assertions are **nested** inside `if ok` and are therefore not emitted at all
against an unpatched kernel, where `phase5_prime_export_host3d_blob` has already
failed. The ftruncate assertion is likewise nested under `if exported`.

### 2c. Expected report counts per host type — read before triaging a run

Phase 5 now emits **12** reports on a full pass. The HOST3D block is guarded by
`if hrc == 0 && hblob.bo_handle != 0` and otherwise prints
`(no host3d blob on this host - skipping ...)` with **no reports at all**, so the
total is host-dependent:

| Host | Phase-5 reports | venustest total | HOST3D block |
|---|---|---|---|
| Linux box, `virtio-gpu-gl-pci,venus=on` | **12** | **80** | runs — all four HOST3D assertions live |
| Mac, plain `virtio-gpu-pci` (no Venus) | **8** | **76** | **skipped silently** |

Baseline before this patch is 68. TODO item 5's recorded `68 → 77` assumed 9 new
reports on a Venus host; with the three added assertions it is **68 → 80** on
Venus and **68 → 76** without.

**A run reporting 76 has not tested the safety argument.** Four assertions —
`phase5_prime_export_host3d_blob`, `phase5_host3d_export_is_not_mappable`,
`phase5_host3d_export_reads_short`, `phase5_host3d_export_refuses_write` —
disappear with no FAIL and no contribution to the failure count. Missing lines
must never be read as passing lines. Grep the log for
`phase5_host3d_export_is_not_mappable` explicitly; if it is absent, the
page-less-export hazard was not exercised at all.

### 2d. The "must fail unpatched" set, stated precisely

Exactly **two** subtests are backed out at HEAD and must FAIL against an
unpatched kernel:

1. `phase5_prime_export_guest_blob`
2. `phase5_prime_export_host3d_blob`

Everything else in phase 5 is either nested behind those two — and so not
emitted at all unpatched — or is a non-regression guard.

**`phase5_other_open_export_refused` is NOT in that set.** It passes at HEAD
too, because HEAD refuses *every* blob export. It is falsifiable only against
its own mutation: replacing `blob_lookup(handle, open_id)` with a bare
`BLOB_BUFFERS.lock().get(&handle)`, which lets fd_b export fd_a's BO. Reading
its PASS as evidence the patch is live would be the `memfd_inflight_close`
error one level up.

### One behavioural risk for the run (not a subtest issue)

The `vmo_acquire_frames` immutability guard changes the **dumb** path too: an
mmap of a dmabuf fd at a non-zero offset whose `first + n` exceeds `1<<order`
previously grew the list (leaking) and now returns ENOMEM. That is the correct
semantic and matches Linux, but it is exactly what `drmsmoke`'s
`PRIME_MMAP_ALIAS` staying PASS is the gate for — and drmsmoke 22/0 is the one
thing checkable locally on the Mac.

---

## 3. Stacking — checked by applying, not by inspection

All combinations were applied for real on top of `a0f2c46` and compared by
`git write-tree` hash. **Every combination applies cleanly and every pair of
orders converges on an identical tree.** No conflicts in either direction.

| Sequence | Applies | Resulting tree |
|---|---|---|
| prime alone | clean | `666c81f8c0a58e583ed484fa7df88fc24563ad3d` |
| prime → blob_cacheability | clean | `712375c8275149c42c5cdb20de7aed458a3019f0` |
| blob_cacheability → prime | clean | `712375c8275149c42c5cdb20de7aed458a3019f0` |
| prime → fb_damage | clean | `715d9611b073e0f4dbbb1f49dabbd94bc96bb3d1` |
| fb_damage → prime | clean | `715d9611b073e0f4dbbb1f49dabbd94bc96bb3d1` |
| prime → blob_cacheability → fb_damage | clean | `dcd07a19c7fb151668ca21487549eb6056b33645` |
| fb_damage → blob_cacheability → prime | clean | `dcd07a19c7fb151668ca21487549eb6056b33645` |

Patches used: `../m9-blob-cacheability/blob_cacheability.patch` and
`../m9-fb-damage-clips/fb_damage_worktree_20260806.patch` (the worktree-refreshed
variant, not the older `fb_damage_clips.patch`).

The specific hazard the brief called out is **confirmed absent**. An earlier
draft of this patch *deleted* `dumb_buffer_phys_order`, whose doc comment
`fb_damage` uses as trailing hunk context, and conflicted both ways. The design
that shipped keeps that function and calls it from `prime_export_backing`, so
the trailing context survives and both fb_damage orders apply clean.

Cross-lane note: the item-9 diagnostic has come back saying the compositor
damages ~96.7% of the output every frame, so the kernel half of FB_DAMAGE_CLIPS
wins nothing and that patch may never land. This changes nothing here — the two
patches touch disjoint code and commute either way.

Worktree was restored to prime-only afterwards (tree `666c81f8`, re-verified).

### Patch provenance

`prime_handle_to_fd_built_20260806.patch` was regenerated with `git diff` from
the built, working worktree. Its `--numstat` is identical to the original's:
`58/4`, `18/4`, `78/19`, `150/0` — i.e. **+304/−27**. (TODO item 5's recorded
"+308/−31" counted four diff headers as content; the patch has not drifted.)

Independently confirmed by the coordinator: the rebuilt patch differs from
`prime_handle_to_fd.patch` in **exactly one line** — the `handle_ftruncate` hunk
header `@@ -5037,6 +5089,13 @@` becoming `@@ -5055,6 +5107,13 @@`, matching the
+18 offset reported in section 1. Everything else is byte-identical.

**A 308-line patch that had never once been compiled needed zero code changes to
build clean on both architectures.**

### Second pass — three added assertions (same day)

The patch now also carries the three subtests from 2b. Changes are confined to
`userland/venustest/src/main.rs`; the three kernel/driver/vfs files are
byte-identical to the first-pass patch.

* **Rebuild: still clean, both arches.** `./scripts/build-all.sh --arch aarch64`
  and `--arch x86_64` both exit 0 with **0 errors**, through to
  `🎉 Build Complete!`. `venustest` still generates exactly its two pre-existing
  warnings (`main.rs:54` unused `c_uint` alias, `main.rs:82`
  `SUPPORTED_CAPSET_IDs` casing) — **no new warnings** from the added code, and
  the two new externs (`read`, `ftruncate`) resolved at link time.
* **Size:** `--numstat` is now `58/4`, `18/4`, `78/19`, `195/0` = **+349/−27**
  (venustest grew by 45 lines).
* **Stacking re-checked, narrowly and deliberately.** Neither
  `blob_cacheability.patch` (touches `arch/`, `drivers/`, `kernel/`, `mm/`,
  `servers/drm/`) nor `fb_damage_worktree_20260806.patch` (touches
  `drivers/src/drm/device.rs` and `drivers/src/drm_device_interface.rs`) touches
  `userland/venustest/src/main.rs` at all, so no stacking check involves the
  changed file. The converged-tree results above therefore cannot have changed
  and were not re-run. As a cheap confirmation, the full three-way stack was
  re-applied in both extreme orders with the updated patch:

  | Sequence | Applies | Resulting tree |
  |---|---|---|
  | prime → blob_cacheability → fb_damage | clean | `61d2a0557b3dd33750fb3560cc14610bb8cf9321` |
  | fb_damage → blob_cacheability → prime | clean | `61d2a0557b3dd33750fb3560cc14610bb8cf9321` |

  Identical, as before. (The hash differs from the first pass only because the
  venustest file now contains 45 more lines.)

Worktree left at prime-only, uncommitted, matching the regenerated patch.
