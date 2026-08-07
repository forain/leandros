# dmabuf lifetime (Stages 1+2) — Mac verification

Lane T, 2026-08-06. Worktree `.claude/worktrees/agent-ac509e37ac74a1da5`, base `a0f2c46`.
**Status: COMPLETE.** Verdict in §9; gaps stated as gaps in §8.

## 0. Tree under test

Apply order per the brief, both clean, no fuzz:

```
$ git apply --check notes/m9-prime-export/prime_handle_to_fd_built_20260806.patch  -> OK
$ git apply         notes/m9-prime-export/prime_handle_to_fd_built_20260806.patch
$ git apply --check notes/m9-dmabuf-lifetime/dmabuf_lifetime.patch                 -> OK
$ git apply         notes/m9-dmabuf-lifetime/dmabuf_lifetime.patch
 M drivers/src/drm_device_interface.rs
 M kernel/src/syscall.rs
 M servers/drm/src/lib.rs
 M servers/vfs/src/lib.rs
 M userland/venustest/src/main.rs
```

Confirms Lane Q's stated apply order: the lifetime patch does not apply to bare `a0f2c46`.

## 1. Scope on this host — what can and cannot be reached

Established by the previous lane (`m9-prime-export/mac-verify-20260806.md` §2.2) and not
re-derived: QEMU 11.0.2 on macOS advertises no `VIRTIO_GPU_F_RESOURCE_BLOB`
(`-device virtio-gpu-pci,blob=on` -> *"need rutabaga or udmabuf for blob resources"*;
udmabuf is Linux-only; rutabaga not compiled in; no `virtio-gpu-gl-pci`). Therefore
`param RESOURCE_BLOB = 0`, no blob BO can be created, and **the entire blob half of
phase 6 is unreachable here.**

The **dumb** half is fully reachable, and it is where the regression risk lives
(cosmic-comp exports dumb buffers). So the A/B in §3 mutates the **dumb** arm.

Re-confirmed first-hand in this lane's own logs rather than taken on trust, because the
whole scoping of the report depends on it:

```
param 3D_FEATURES   = 0
param RESOURCE_BLOB = 0
param HOST_VISIBLE  = 0
param CONTEXT_INIT  = 0
[GPU] resource_create_blob refused: no RESOURCE_BLOB
```

## 1bis. A second worktree-environment gap, and the control that caught it

Recorded for every future worktree lane, alongside the previous lane's
`brush`/`coreutils`/`bottom-leandros` symlink finding.

`driver.py` resolves the UEFI NVRAM file as `$REPO_ROOT/aarch64_vars.fd`, and it
**auto-creates only the x86_64 one** (`shutil.copyfile` at driver.py:281 exists in the
x86_64 branch; the aarch64 branch at driver.py:233 just names the path). A fresh
worktree has no `aarch64_vars.fd` (it is `.gitignore`d, line 14), so QEMU exits at once:

```
qemu-system-aarch64: -drive if=pflash,unit=1,format=raw,file=.../aarch64_vars.fd:
    Could not open '.../aarch64_vars.fd': No such file or directory
```

`driver.py start` still prints `QEMU started (PID …)` and then `WARNING: shell prompt not
seen. Serial tail:` **followed by nothing**. The serial log is 0 bytes. This is exactly the
shape the brief warns about: a run that produces no test lines at all, which a grep-based
reader would score as "tests ran and emitted nothing".

**The positive control is what caught it.** The first command of the session was
`nosuchbinary_xyz42`, and instead of the expected `error: command not found` it produced

```
ConnectionRefusedError: [Errno 61] Connection refused
```

i.e. the harness could not even reach the guest. Had the control been omitted, the four
following commands would each have produced an empty file and every assertion would have
read as UNEXERCISED — or, worse, as a clean run.

Fixed by `cp /opt/homebrew/share/qemu/edk2-arm-vars.fd aarch64_vars.fd` (a blank NVRAM
template; Limine lives at the removable-media fallback path `/EFI/BOOT/BOOTAA64.EFI`, so
no boot entry is needed). The guest then boots to `login:` normally.

Image sanity, checked before trusting any boot (the previous lane's trap):

```
Packed brush     (size: 6122896 bytes)     <- /bin/brush IS present
Packed vfstest, scmtest, drmsmoke, venustest
grep -c "brush source not found" -> 0
```

## 2. Arm A — patched (correct) kernel, aarch64

Harness: `scripts/scmrun.py` (one process per command, explicit pre-send drain, no
`expect()`, raw serial dumped verbatim). Positive control was the first command of the
boot and behaved:

```
$ scmrun.py "nosuchbinary_xyz42" 8
error: command not found: nosuchbinary_xyz42
```

### 2.1 venustest phase 6, literal report lines

```
phase6_guest_blob_created: FAIL
phase6_dumb_created: PASS
phase6_dumb_pattern_stamped: PASS
phase6_dumb_exported: PASS
phase6_dumb_payload_survives_destroy: PASS
phase6_dumb_alloc_after_release: PASS
--- venustest done, failures = 32 ---
```

16 PASS / 32 FAIL / 48 reports. That reconciles exactly with the previous lane's
PRIME-only Mac baseline of 11/31 = 42 reports: phase 6 adds 6 reports here, 5 passing
(the dumb half) and 1 failing (`phase6_guest_blob_created`, unreachable on this host).

`grep -c "no BLOB_OBJS getparam"` = **0**, i.e. `VIRTGPU_PARAM_LEANDROS_BLOB_OBJS`
answered. That param is introduced by this patch, so its presence is an independent
confirmation the lifetime patch is in the running kernel — but it is only a
*presence* check, not a behaviour check. The behaviour check is §3.

### 2.2 The instrument, and what it shows

A throwaway trace was added to `drivers/src/drm_device_interface.rs` for this lane only
(**not proposed for landing**, reverted afterwards, present **identically in both A/B
arms** so it is not a variable). It prints one line per dumb-BO lifecycle event with the
physical address, plus a rate-limited `[DUMBWATCH]` census. Call sites:
`DrmDumbBuffer::create`, `free_dumb`, `dumb_unref_by_obj`, `prime_export_acquire`. Every
call site invokes it with no map guard held.

Literal trace of the phase-6 dumb half on the patched kernel, in order:

```
[DWA] create_dumb      phys=0x0000000047526000 ord=0x00000000
[DWA] export_dumb      phys=0x0000000047526000 ord=0x00000000
[DWA] retire_KEEP      phys=0x0000000047526000 ord=0x00000000
[DWA] create_dumb      phys=0x000000004753E000 ord=0x00000000     <- churn 1
[DWA] create_dumb      phys=0x00000000B82D2000 ord=0x00000000     <- churn 2
[DWA] create_dumb      phys=0x00000000BA305000 ord=0x00000000     <- churn 3
[DWA] create_dumb      phys=0x00000000BA302000 ord=0x00000000     <- churn 4
[DWA] create_dumb      phys=0x00000000BA300000 ord=0x00000000     <- churn 5
[DWA] create_dumb      phys=0x00000000BC503000 ord=0x00000000     <- churn 6
[DWA] create_dumb      phys=0x00000000B8200000 ord=0x00000000     <- churn 7
[DWA] create_dumb      phys=0x0000000047461000 ord=0x00000000     <- churn 8
[DWA] free_at_destroy  phys=0x000000004753E000 ord=0x00000000     <- churn teardown
[DWA] free_at_destroy  phys=0x00000000B82D2000 ord=0x00000000
[DWA] free_at_destroy  phys=0x00000000BA305000 ord=0x00000000
[DWA] free_at_destroy  phys=0x00000000BA302000 ord=0x00000000
[DWA] free_at_destroy  phys=0x00000000BA300000 ord=0x00000000
[DWA] free_at_destroy  phys=0x00000000BC503000 ord=0x00000000
[DWA] free_at_destroy  phys=0x00000000B8200000 ord=0x00000000
[DWA] free_at_destroy  phys=0x0000000047461000 ord=0x00000000
[DWA] free_at_fd_close phys=0x0000000047526000 ord=0x00000000     <- close(fd) frees it
[DWA] create_dumb      phys=0x0000000047526000 ord=0x00000000     <- alloc_after_release
[DWA] free_at_destroy  phys=0x0000000047526000 ord=0x00000000
```

Read directly off that:

1. `retire_KEEP` — `DESTROY_DUMB` retired the gem handle and **did not** free the block.
   That is the entire behavioural change, observed rather than argued.
2. The eight churn allocations returned eight addresses and **none of them is
   `0x47526000`** — the test buffer was genuinely still held.
3. `free_at_fd_close` — the block is freed by `close(fd)`, exactly once, on the
   `vmo_free_slot` -> `bo_release_exported` -> `dumb_unref_by_obj` path. No
   `[DRM] bo refcount underflow` anywhere in the log.
4. `[DUMBWATCH] ev=0x10 len=0x4 retired=0x1 cre=0x9 exp=0x1 free=0x5 blobobjs=0x0` —
   the census caught the window with exactly one retired-but-pinned record live, and
   `blobobjs=0` confirms no blob object exists on this host.

### 2.3 The churn is NOT vacuous — proved from this arm's own trace

The brief flags this as the thing that could make the test pass against its own bug.
The patched arm answers it without needing the mutated arm at all:

**`free_at_fd_close phys=0x47526000` is immediately followed by
`create_dumb phys=0x47526000`.** The buddy allocator's `free` ends in `push_front` and
`alloc` pops from the head (`mm/src/buddy.rs`: `free` -> `push_front(lists, order, addr)`;
`alloc` -> `lists[o].head.take()`), so a freed order-0 page is returned to the *very next*
order-0 allocation. That is measured here, not assumed.

Therefore, on a kernel that frees at `DESTROY_DUMB` instead of at `close(fd)`, churn
allocation #1 necessarily receives `0x47526000` and `DrmDumbBuffer::create` zeroes it
(`ptr::write_bytes(virt, 0, size)`, and `size == 4096 == 1<<order pages` here, so the
whole block). The pattern cannot survive. §3 confirms this on hardware.

### 2.4 Non-regression, arm A (patched), aarch64, fresh image, vfstest run exactly once

| Test | measured | expected | verdict |
|---|---|---|---|
| `vfstest`  | **36 PASS / 0 FAIL** | 36/0 | unmoved |
| `scmtest`  | **30 PASS / 0 FAIL** | 31/0 per the brief — see below | unmoved *for this base* |
| `drmsmoke` | **22 PASS / 0 FAIL** | 22/0 | unmoved |
| `venustest`| 16 PASS / 32 FAIL | — | see §2.1 |

`drmsmoke`'s three PRIME lines: `PRIME_HANDLE_TO_FD: PASS`, `PRIME_MMAP_ALIAS: PASS`,
`PRIME_FD_TO_HANDLE: PASS`.

**The scmtest 30-vs-31 discrepancy is a base mismatch, not a regression.** The brief says
scmtest is 31/0 now because TIME_WAIT landed as `fe411ff`. That commit is **not an
ancestor of this lane's base**:

```
$ git log --oneline -1
a0f2c46 TODO: reconcile after the EMFILE, listen and init-server landings
$ git merge-base --is-ancestor fe411ff HEAD ; echo $?
1
```

So this tree has 30 scmtest subtests and scores 30/0. The lifetime patch touches no
socket code. Whoever stacks this onto a base containing `fe411ff` should expect 31/0
there.

## 3. Arm B — the falsifiability mutation, and the A/B

### 3.1 Which arm was mutated, and why

The author's named mutation is the **blob** arm of `prime_export_acquire`. That arm is
**unreachable on this host** (§1): no blob BO can be created, so `prime_export_acquire`
never takes its blob branch and deleting the line would change nothing observable. Per
the brief, the A/B was therefore done on the **dumb** arm, which is the exact structural
twin and the half that carries the compositor regression risk.

`drivers/src/drm_device_interface.rs`, `prime_export_acquire`, dumb branch — one line
commented out and nothing else:

```rust
    let b = dumb.get_mut(&handle).filter(|b| b.handle_live)?;
    // LANE T FALSIFIABILITY MUTATION — the dumb twin of the blob-arm line the
    // author named. Reverted after the A/B.
    // b.refs = b.refs.saturating_add(1);
    let e = PrimeExport {
```

### 3.2 Proof the image was identical across the two arms

Same image, kernel-only rebuild (`scripts/m7z2-kernel-only.sh aarch64`, which rebuilds
only the Limine kernel and re-embeds it; it does not touch userland, initrd or the f2fs
data images). Hashes taken immediately **before** and immediately **after** the arm-B
kernel rebuild:

```
                                    before m7z2                       after m7z2
f2fs-data0-aarch64.img              3dfc0004d510d5c781443931be23fdb3  3dfc0004d510d5c781443931be23fdb3   IDENTICAL
f2fs-data1-aarch64.img              fe19e71025552b053972cf976188b5b8  fe19e71025552b053972cf976188b5b8   IDENTICAL
userland/.../release/venustest      c002f94e59cc50469275a28f56ae0429  c002f94e59cc50469275a28f56ae0429   IDENTICAL
leandros-limine-aarch64.img         e7499a2666b85c4e3638445eb190d7ae  83c5b3c207b6b0b1434d8aea51f0a78d   CHANGED (the kernel)
```

So the `venustest` binary the two arms executed was bit-identical and the kernel was the
only variable. (The data images differ from the pristine post-build hash `5d78dc95…`
because the guest writes to them — f2fs is persistent. What matters is that they did not
change *across the arms*, which is what is shown above. Arm B therefore did **not**
re-run `vfstest`; the once-per-image rule is intact.)

### 3.3 The A/B result — literal lines from both kernels

Positive control was the first command of the arm-B boot as well, and passed
(`error: command not found: nosuchbinary_xyz42`, `grep -c` = 1).

| | arm A (patched) | arm B (one line deleted) |
|---|---|---|
| `phase6_guest_blob_created` | FAIL (unreachable here) | FAIL (unreachable here) |
| `phase6_dumb_created` | PASS | PASS |
| `phase6_dumb_pattern_stamped` | PASS | PASS |
| `phase6_dumb_exported` | PASS | PASS |
| **`phase6_dumb_payload_survives_destroy`** | **PASS** | **FAIL** |
| `phase6_dumb_alloc_after_release` | PASS | PASS |
| `venustest done, failures =` | 32 | 33 |
| `[DRM] bo refcount underflow` in serial | **0 occurrences** | **1 occurrence** |
| `[DWA] retire_KEEP` in serial | **1 occurrence** | **0 occurrences** |
| `drmsmoke` | 22 PASS / 0 FAIL | 22 PASS / 0 FAIL |

Arm B's extra diagnostic lines, verbatim:

```
  dumb payload lost at offset 0
phase6_dumb_payload_survives_destroy: FAIL
[DRM] bo refcount underflow obj=0x00000001
```

**Two independent signals from one deleted line**, exactly as the author predicted for
the blob twin: the payload assertion and the underflow log. (The author's third signal,
`phase6_objs_survive_close`, is a blob-only counter assertion and is unreachable here —
the dumb half has no object counter. Its dumb-side equivalent is the `retire_KEEP` /
`free_at_destroy` flip in the instrument trace, which is the fourth row above.)

**Triage note honoured:** the failure presented as a **wrong value**, not an error code
and not a panic. `read()` returned the full byte count and the bytes were wrong.
`grep -c "panicked"` matches once in both arms, and it is venustest's own source string
`(the outside-RAM device VMA that panicked the kernel needs a 3D host)` — not a kernel
panic. No fault, no exception, no `FAR=` in either arm.

`phase6_dumb_alloc_after_release: PASS` in arm B is correct and expected: the mutation
frees the block once (at `DESTROY_DUMB`) and then fails to free it at `close(fd)`, so no
double free reaches the allocator. That assertion is the double-free detector, and the
mutation is not a double free.

### 3.4 The churn genuinely forces reallocation — measured, in arm B

Arm B's instrument trace over the phase-6 dumb half, verbatim and in order:

```
[DWA] create_dumb     phys=0x00000000B82D2000 ord=0x00000000   <- the test buffer
[DWA] export_dumb     phys=0x00000000B82D2000 ord=0x00000000   <- PRIME_HANDLE_TO_FD, no ref taken
[DWA] free_at_destroy phys=0x00000000B82D2000 ord=0x00000000   <- DESTROY_DUMB FREES IT (the UAF)
[DWA] create_dumb     phys=0x00000000B82D2000 ord=0x00000000   <- churn #1 GETS THE SAME PAGE
[DWA] create_dumb     phys=0x00000000BA305000 ord=0x00000000   <- churn 2
[DWA] create_dumb     phys=0x00000000BA302000 ord=0x00000000   <- churn 3
[DWA] create_dumb     phys=0x00000000BA300000 ord=0x00000000   <- churn 4
[DWA] create_dumb     phys=0x00000000BC503000 ord=0x00000000   <- churn 5
[DWA] create_dumb     phys=0x00000000B8281000 ord=0x00000000   <- churn 6
[DWA] create_dumb     phys=0x0000000047526000 ord=0x00000000   <- churn 7
[DWA] create_dumb     phys=0x00000000B8200000 ord=0x00000000   <- churn 8
```

**Churn iteration #1 received `0xB82D2000` — the exact page freed one line earlier — and
`DrmDumbBuffer::create` zeroed it.** That is why `read(fd)` came back
`dumb payload lost at offset 0`. The churn loop is **not** padding and the test is **not**
vacuous: without it the freed page would very likely still have held the pattern
(`mm::buddy::free` does not scrub), and the test would have passed against its own bug.

Note also that arm B has **no `retire_KEEP` line at all**, and arm A's `retire_KEEP`
appears exactly where arm B has `free_at_destroy`. The single deleted line moves the
`mm::buddy::free` from `close(fd)` to `DESTROY_DUMB`, which is precisely the defect
described in `dmabuf_lifetime.md` §1.

**Verdict on Priority 1: the fix is proven live on this hardware, on the dumb arm.**

## 4. Which assertions RAN, and which were UNEXERCISED here

Stated explicitly because absent lines must never read as passes. Every "UNEXERCISED"
row below was verified by `grep`ping the name over **every** log in this run and getting
nothing, on **both** kernels.

### Phase 6 — ran on this host

| Assertion | arm A | arm B |
|---|---|---|
| `phase6_dumb_created` | PASS | PASS |
| `phase6_dumb_pattern_stamped` | PASS | PASS |
| `phase6_dumb_exported` | PASS | PASS |
| `phase6_dumb_payload_survives_destroy` | PASS | **FAIL** |
| `phase6_dumb_alloc_after_release` | PASS | PASS |
| `phase6_guest_blob_created` | FAIL — this is the "no blob on this host" gate, not a defect |

### Phase 6 — UNEXERCISED here, never emitted on either kernel

All of these sit inside `if made {` on `phase6_guest_blob_created`, which cannot succeed
without `VIRTIO_GPU_F_RESOURCE_BLOB`:

| Assertion | Status |
|---|---|
| `phase6_create_adds_one_object` | **UNEXERCISED** — never emitted |
| `phase6_pattern_stamped` | **UNEXERCISED** — never emitted |
| `phase6_exported` | **UNEXERCISED** — never emitted |
| `phase6_export_adds_no_object` | **UNEXERCISED** — never emitted |
| `phase6_objs_survive_close` | **UNEXERCISED** — never emitted |
| `phase6_payload_survives_close` | **UNEXERCISED** — never emitted |
| `phase6_mmap_of_fd_still_coherent` | **UNEXERCISED** — never emitted |
| `phase6_objs_zero_after_fd_close` | **UNEXERCISED** — never emitted |
| `phase6_alloc_after_release` | **UNEXERCISED** — never emitted |

Consequences worth naming rather than glossing:

* **The `VIRTGPU_PARAM_LEANDROS_BLOB_OBJS` counter was never asserted on.** It was proved
  to *exist* (`grep -c "no BLOB_OBJS getparam"` = 0, and it reported `blobobjs=0x0`), but
  no test compared it before/after anything. The whole counter half of phase 6 —
  the thing the design says is the only way to distinguish "the fd kept the buffer alive"
  from "the read found plausible bytes" — is untested here. On the dumb path that
  distinction was instead established by the physical-address trace (§3.4).
* **`blob_unref`, `BlobObj`, the `BLOB_OBJS`/`BLOB_BUFFERS` object/handle split, the
  `hostvis_map_blob` rollback rekeying, and `free_blob`'s lock-scope fix are all
  UNEXERCISED.** No blob object is ever created on this host, so none of that code runs.
* **`phase6_mmap_of_fd_still_coherent` has no dumb twin at all** — the dumb half asserts on
  `read()` only. The `mmap(MAP_SHARED)` half of the hazard is therefore **not covered
  anywhere in this run**, on either path.
* The gates named in `dmabuf_lifetime.md` §10 that need a Venus host — `vkrender`
  (`s2_checksum = 0x02C0FDC5`) and `vktest` — were **not run**.

## 5. Priority 2 — the leak watch

### 5.1 First attempt, and why its silence was NOT a result

A 200 s COSMIC session (`sh /bin/start-cosmic-leandros >/tmp/s.log 2>&1 &`, launched
through `scmrun.py`, positive control passed first) produced **1692 bytes of serial in
200 s, 11 dumb-BO events, and zero `[DUMBWATCH]` census lines** — the census was
rate-limited to one line per 16 events and 11 events never reached the threshold.

That is an **absent measurement, not a clean one**, and it is exactly the failure the
brief warns about: a quiet serial line looks identical to a steady-state one. It cannot
distinguish "no buffers leaked" from "the session stopped allocating" from "the session
died". It is reported here as a discarded attempt, not as evidence.

It did, however, settle one premise that the whole leak risk depends on:

```
[DWA] create_dumb  phys=0x00000000A9400000 ord=0x0000000A
[DWA] export_dumb  phys=0x00000000A9400000 ord=0x0000000A
[DWA] create_dumb  phys=0x00000000A9800000 ord=0x0000000A
[DWA] export_dumb  phys=0x00000000A9800000 ord=0x0000000A
[DWA] create_dumb  phys=0x00000000A9C00000 ord=0x0000000A
[DWA] export_dumb  phys=0x00000000A9C00000 ord=0x0000000A
[DWA] free_at_destroy phys=0x00000000A9400000 ord=0x0000000A
[DWA] create_dumb  phys=0x0000000057800000 ord=0x0000000A
[DWA] export_dumb  phys=0x0000000057800000 ord=0x0000000A
[DWA] create_dumb  phys=0x00000000AF144000 ord=0x00000002
[DWA] export_dumb  phys=0x00000000A9C00000 ord=0x0000000A
```

**cosmic-comp really does `PRIME_HANDLE_TO_FD` its dumb scanout buffers** — every
`create_dumb` at order 0xA (1024 pages = 4 MiB, a full-screen scanout) is followed
immediately by an `export_dumb` of the same page, and `0xA9C00000` is exported twice. The
order-2 (16 KiB) allocation is the 64x64 cursor plane. So the code path this change alters
**is** on the compositor's path, and the author's risk is a real risk rather than a
hypothetical one. Note also that a *single* leaked scanout buffer here costs **4 MiB**, so
this is worth a real number.

What it does **not** show is a rate: 11 events in 200 s means the compositor is not
exporting per frame at a high rate in this window, but with no time anchor it is
impossible to tell whether those 11 events were spread over 200 s or all happened in the
first two seconds and the session then stalled.

### 5.2 Second attempt — a TIME-driven census

The instrument was changed so silence becomes readable: a `[BOCENSUS]` line is emitted
from `drm_tick()` (the existing 100 Hz hook) every ~500 ticks (~5 s), **unconditionally**
and independent of DRM activity, carrying the tick counter, `DUMB_BUFFERS.len()`, the
number of those records that are **retired-but-pinned** (`handle_live == false`, i.e. kept
alive only by an exported fd — the leak this change could introduce), `BLOB_OBJS.len()`,
and running create/export/free totals. It takes `DUMB_BUFFERS` and no other lock.

`dumb_retired` climbing monotonically is the leak signature. `dumb_len` climbing with
`dumb_retired` flat would be the compositor simply holding more buffers.

### 5.3 The numbers — 38 samples over ~185 s of live COSMIC session

Positive control passed first (`error: command not found: nosuchbinary_xyz42`).
`[BOCENSUS]` sample count: **38**. `t` is the 100 Hz tick counter, so `t=0x3E8` = 1000
ticks = 10 s and `t=0x4C2C` = 19500 ticks = 195 s.

```
[BOCENSUS] t=0x000003E8 dumb_len=0x0 dumb_retired=0x0 blob_objs=0x0 cre=0x0 exp=0x0 free=0x0
[BOCENSUS] t=0x000005DC dumb_len=0x4 dumb_retired=0x0 blob_objs=0x0 cre=0x5 exp=0x5 free=0x1
[BOCENSUS] t=0x000007D0 dumb_len=0x4 dumb_retired=0x0 blob_objs=0x0 cre=0x5 exp=0x5 free=0x1
   ... 34 further samples, every field IDENTICAL ...
[BOCENSUS] t=0x00004C2C dumb_len=0x4 dumb_retired=0x0 blob_objs=0x0 cre=0x5 exp=0x5 free=0x1
```

and after the watch window closed, two more samples at `t=0x4E20` and `t=0x5014`, then
`t=0x5208`, `t=0x53FC`, `t=0x55F0` (up to ~220 s), all still
`dumb_len=0x4 dumb_retired=0x0 ... cre=0x5 exp=0x5 free=0x1`.

**The numbers, stated plainly:**

| quantity | value | over |
|---|---|---|
| live dumb BOs (`DUMB_BUFFERS.len()`) | **4**, flat | 15 s -> 220 s |
| **retired-but-fd-pinned dumb records** | **0**, flat | the entire session |
| live blob objects | **0** (no blob support on this host) | entire session |
| cumulative dumb creates | **5**, frozen after 15 s | — |
| cumulative dumb PRIME exports | **5**, frozen after 15 s | — |
| cumulative dumb frees | **1**, frozen after 15 s | — |
| `[DRM] bo refcount underflow` occurrences | **0** | entire session |

**Growth rate: zero.** Not "slow", not "bounded-looking" — the counters are literally
constant for 185+ seconds.

### 5.4 Liveness — why the flat series is a steady state and not a corpse

A flat counter series is worthless without proof the session was still running, and the
brief's own warning about silence applies to me as much as to anyone. `ps` and `grep` are
not present as binaries in this image (both returned `command not found`), so process
liveness was established from the framebuffer instead.

Screenshot taken at the end of the run
(`.../scratchpad/T-leak2-screen.png`): the full COSMIC desktop is composited — the Orion
Nebula wallpaper across the whole 1280x800 output, the dark panel bar along the top, and
**a running clock reading `00:03:54`**. The session had been up ~4 minutes at that point,
so the clock is tracking real time, which means the panel applet is still being scheduled
and the compositor is still presenting frames.

So: the compositor was alive and presenting for the whole window, and during that window
it allocated **no** new dumb buffers and released **none**. It allocates its scanout set
once at startup (5 creates, 5 exports, 1 free -> 4 held) and reuses it.

### 5.5 What this does and does not establish

**Establishes (Priority 2 answered):** keeping dumb buffers alive until their export fd
closes does **not** leak in cosmic-comp's steady state on this host. `dumb_retired`
stayed at **0** for the entire session, so no buffer was ever held alive *solely* by an
exported fd — every exported buffer still had its gem handle open. Steady state is 4
buffers, unchanged from what it would be without this patch.

**Does not establish, and should be said rather than glossed:** because `dumb_retired`
never left 0, the compositor session **never entered the new retention path at all**.
The measurement shows the change is *inert* on this workload — which is the reassuring
answer, and the one that matters for regression risk — but it is not a stress test of
retention under churn. It also contradicts the brief's framing that cosmic-comp "exports
per frame": it exports **per buffer, once, at allocation**, and then reuses. On this
host, in this window, there is no per-frame export traffic for the change to accumulate
against.

The retention path itself was exercised, and shown to release correctly, by venustest
phase 6 (§2.2: `retire_KEEP` then `free_at_fd_close`, exactly once, no underflow).

## 6. Harness and controls — which instrument produced each number

Every count in this file comes from `scripts/scmrun.py`: one process per command, an
explicit pre-send socket drain, no `expect()` and therefore no prompt heuristic, reading
for a fixed wall-clock duration and dumping raw serial verbatim. `driver.py` was used
only for `start` / `login` / `stop` / `screenshot` — never for `cmd`, whose prompt
heuristic is the instrument the brief warns returns early and swallows error lines.

Counts were taken by grepping those raw dumps for **literal** `<name>: PASS` /
`<name>: FAIL` substrings emitted by the test binary itself. No harness exit status is
load-bearing anywhere in this report.

**Positive controls.** `nosuchbinary_xyz42` was the first command of **every** boot
(four boots: arm A, arm B, leak watch v1, leak watch v2). It returned
`error: command not found: nosuchbinary_xyz42` in all four. In the very first attempt of
the session it instead returned `ConnectionRefusedError`, which is how the missing
`aarch64_vars.fd` (§1bis) was caught before it could turn into four empty result files
read as passes.

**Control on the absence-checking itself.** The UNEXERCISED table in §4 was not asserted
from reasoning; each name was grepped across every log in the run. The same script also
grepped two names that *should* be present, and found them (2 occurrences each = both
arms), which shows the grep was capable of matching had the others been there.

**Instrument added for this lane, and its status.** A `[DWA]` per-event physical-address
trace and a `[BOCENSUS]` tick-driven census were added to
`drivers/src/drm_device_interface.rs`. They are **throwaway**, **not proposed for
landing**, and were **identical in both A/B arms** so they are not a variable in §3. They
were reverted from the worktree afterwards. Their call sites take `DUMB_BUFFERS` only,
never while another map guard is held.

**One instrument was discarded mid-lane for producing an unreadable result** — the
rate-limited (1-in-16) `[DUMBWATCH]` census, which emitted nothing at all during a 200 s
compositor session because the session only generated 11 events. That is written up in
§5.1 as a discarded attempt rather than quietly replaced.

## 7. Priority 3 — non-regression, both arches

Freshly regenerated images (`build-all.sh` per arch), `vfstest` run **exactly once** per
image, positive control first on every boot, all counts from `scmrun.py` raw dumps.

| Test | aarch64 | x86_64 | expected | verdict |
|---|---|---|---|---|
| `drmsmoke`  | **22 PASS / 0 FAIL** | **22 PASS / 0 FAIL** | 22/0 | unmoved |
| `vfstest`   | **36 PASS / 0 FAIL** | **36 PASS / 0 FAIL** | 36/0 | unmoved |
| `scmtest`   | **30 PASS / 0 FAIL** | **30 PASS / 0 FAIL** | 31/0 in the brief | see below |
| `venustest` | 16 PASS / 32 FAIL | 16 PASS / 32 FAIL | — | identical across arches |

`venustest`'s 32 failures are the pre-existing "no 3D / no blob device on this host" set
(`getparam_3d_features`, `getparam_resource_blob`, `host_advertises_venus_capset`,
`context_init_venus`, `resource_create_blob`, the phase2/3/4/5 context tests, and
`phase6_guest_blob_created`). None of them is introduced by this patch — the previous
lane measured 11/31 on the same host with only the PRIME patch, and phase 6 accounts for
exactly the delta (+5 PASS, +1 FAIL).

**The x86_64 phase-6 dumb half passes identically**, and the instrument confirms the same
mechanism ran there: `retire_KEEP` present once, `[DRM] bo refcount underflow` absent.

```
phase6_dumb_created: PASS
phase6_dumb_pattern_stamped: PASS
phase6_dumb_exported: PASS
phase6_dumb_payload_survives_destroy: PASS
phase6_dumb_alloc_after_release: PASS
```

**scmtest 30 vs the briefed 31 is a base mismatch, not a regression** — see §2.4:
`fe411ff` (TIME_WAIT) is not an ancestor of `a0f2c46`, so this tree has 30 subtests.
Both arches score 30/0, i.e. full marks for this base.

`waittest` was not run; no `waittest` claim is made either way.

## 8. Gaps in this verification, stated as gaps

1. **The blob half is entirely UNEXERCISED** (§4). Nine phase-6 assertions never emitted
   on either kernel, on either arch. The blob-arm mutation the author named was not run
   because the arm is unreachable here.
2. **The `mmap(MAP_SHARED)` half of the hazard is not covered anywhere in this run.**
   `phase6_mmap_of_fd_still_coherent` is blob-only and the dumb half has no `mmap` twin.
   The `read()` half is covered on both arches.
3. **The A/B was run on aarch64 only.** The mutation was not re-run on x86_64. The
   refcount code is architecture-independent (`drivers/`, `servers/vfs/`) and the x86_64
   patched arm passes the same five assertions with the same instrument trace, but the
   flip itself was not re-demonstrated there. Flagged rather than assumed away — the
   same gap the previous lane left.
4. **The leak watch never entered the retention path** (§5.5). It shows the change is
   inert on cosmic-comp's steady state; it is not a churn stress test.
5. **`vkrender` (`s2_checksum = 0x02C0FDC5`) and `vktest` were not run** — they need the
   Venus host.
6. **No `waittest` / `forktest` / `epolltest` etc. sweep** was run.

## 9. Verdict

**Priority 1 — the A/B: PASSED. The fix is proven live on this hardware.**
One commented-out line in `prime_export_acquire`'s dumb arm flips
`phase6_dumb_payload_survives_destroy` from **PASS to FAIL**, adds
`[DRM] bo refcount underflow obj=0x00000001` to the serial log, and removes the
`retire_KEEP` event — three independent signals, same image (data images and the
`venustest` ELF byte-identical, kernel-only rebuild, hashes in §3.2). The failure
presented as a **wrong value**, not a panic, exactly as the author predicted.

**The churn is load-bearing and the test is not vacuous.** Measured, not argued: in the
mutated arm, churn allocation #1 received `0xB82D2000` — the exact page freed one event
earlier — and zeroed it. The buddy allocator's LIFO free list makes this deterministic,
and the patched arm shows the same property independently
(`free_at_fd_close 0x47526000` immediately followed by `create_dumb 0x47526000`).

**Priority 2 — the leak watch: NO LEAK, with a number.** Over a live 185+ s COSMIC
session (liveness proved by a composited desktop with a running `00:03:54` clock),
retired-but-fd-pinned dumb records stayed at **0**, live dumb buffers at **4**, and
cumulative creates/exports/frees frozen at **5 / 5 / 1**. Growth rate is literally zero,
not merely bounded. Zero `bo refcount underflow`. The honest caveat is that the
compositor exports **per buffer at allocation, not per frame**, so the retention path was
never entered during the session — the change is inert here rather than stress-tested.

**Priority 3 — non-regression: CLEAN, both arches.** drmsmoke 22/0, vfstest 36/0,
scmtest 30/0 (correct for this base), venustest identical across arches.

**Is it safe to sync the box commits?** **Yes, on the Mac-checkable evidence** — nothing
regresses on either arch, the dumb-path refcount is demonstrated live and correct, and
the compositor steady state is unaffected. But this must **not** be recorded as fully
verified: the blob half — `BlobObj`, `blob_unref`, the object/handle split, the
`BLOB_OBJS` counter assertions, and the `mmap`-of-exported-fd hazard — remains a
source-level claim only. Those need the Linux box
(`virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G -display egl-headless`).

## 10. Worktree state left behind

`.claude/worktrees/agent-ac509e37ac74a1da5`, **not committed**, both patches applied,
mutation reverted (verified: `git diff` of the restored tree is byte-identical to the
arm-A diff, `diff` rc 0). The throwaway `[DWA]` / `[BOCENSUS]` instrument is still in the
tree at the time of writing and is **not for landing** — it is 4 call sites plus two
helpers in `drivers/src/drm_device_interface.rs`, all marked `LANE T THROWAWAY`.

`aarch64_vars.fd` was created in the worktree from
`/opt/homebrew/share/qemu/edk2-arm-vars.fd`. It is `.gitignore`d. Every future aarch64
worktree lane needs this, or QEMU exits instantly with an empty serial log (§1bis).

Raw logs: `/private/tmp/claude-501/-Users-forain-code-leandros/b625f53e-1f90-454e-8d4a-e4da9aae5da2/scratchpad/`
(`T-aaA-*`, `T-aaB-*`, `T-leak-*`, `T-leak2-*`, `T-x86A-*`, `T-build-*`, `T-kern-*`),
plus the liveness screenshot `T-leak2-screen.png`.
