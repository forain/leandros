# K1 — Shared file-backed mmap (shared VMO), minimal-diff design

Scope of this doc: make `MAP_SHARED` of **tmpfs/memfd** files genuinely shared
(same physical pages across processes, coherent with `read()`/`write()`),
enforce `F_SEAL_SHRINK`, and keep `ftruncate` sane — with the smallest safe
change. f2fs `MAP_SHARED` stays degraded (private copy). Acceptance: scmtest
subtests 3 (`shared_memfd_pixels`) and 4 (`seals`) flip to PASS; vfstest /
polltest / forktest / memtest stay green.

---

## 0. Survey — what exists today (cited)

**No cross-process shared memory exists at all.** `MAP_SHARED|MAP_ANONYMOUS`
does *not* share pages between unrelated processes — each address space owns
its own per-page frame vector (`VmaRegion.lazy_pages`, `mm/src/vmm.rs:45`).
The `is_shared` flag threaded through `map_lazy` (`mm/src/vmm.rs:340,373`) only
changes **fork** behavior: `clone_as` shares full-permission frames + refcounts
them for a `MAP_SHARED` region vs. CoW-downgrading a private one
(`mm/src/cow.rs:63,173,193`). So the "existing shared-anonymous machinery keyed
on the file object" the brief references is really two reusable primitives:

- **`mm::pageref`** (`mm/src/pageref.rs`) — per-physical-frame refcount with
  the "untracked ⇒ refcount 1 (sole owner)" convention; `unref_or_free` frees a
  frame back to buddy exactly when the last owner releases it. Already driven by
  `unmap_range` (`vmm.rs:688`), `AddressSpace::drop` (`vmm.rs:154`), the CoW
  promotion (`vmm.rs:512`), and `clone_as`'s inc sweep (`cow.rs:170,188`).
- **`clone_as`'s `MAP_SHARED` branch** — already shares full-perm frames across
  fork and pageref-incs them (`cow.rs:184-201`). It keys on
  `map_flags & MAP_SHARED`, **not** on `file_cap`, and skips the
  `file_cap == usize::MAX` device sentinel (`cow.rs:64`). This means a lazy VMA
  whose `lazy_pages` are pre-populated with borrowed frames and whose
  `map_flags` has `MAP_SHARED` set is forked/unmapped/torn-down **correctly by
  code that already exists** — no changes to fork, munmap, exit, or the fault
  handler.

**File-backed mmap today = eager private copy.** `sys_mmap`'s file path
(`kernel/src/syscall.rs:1427-1568`) does `as_.map()` (eager contiguous alloc,
`vmm.rs:190`) then loops `VFS_READ` copying bytes into the fresh frames
(`syscall.rs:1537-1544`). The comment at `syscall.rs:1436` states the
degradation explicitly. `map_lazy_file` (`vmm.rs:392`) exists but is only used
by the ELF loader (private, demand-read via the `FileReadFn` hook), never by
`sys_mmap`. The `FileReadFn`/`FileRefFn` hooks (`vmm.rs:79-122`,
registered `syscall.rs:154`) **copy bytes**, they do not expose the file's
physical frame — so they cannot be reused for sharing as-is.

**tmpfs / memfd storage = inline `[u8; 32768]`.** A tmpfs "inode" is a
`TmpFileEntry` in the static `TMP_FILES` array (`vfs/src/lib.rs:260-312`), whose
bytes live in `data: [u8; MAX_TMP_SIZE]` (`MAX_TMP_SIZE = 32768`,
`lib.rs:257`). read/write/ftruncate/fstat all touch `entry.data`/`entry.len`
directly:
- read: `vfs/src/lib.rs:2349-2367` (`entry.data[cur..]`, bounded by `entry.len`)
- write: `lib.rs:2435-2460` (`entry.data`, grows `entry.len`, capped at 32K)
- ftruncate: `lib.rs:3834-3847` (zero-fill `entry.data`, set `entry.len`)
- fstat size: `lib.rs:5117` TmpFile arm returns `entry.len`
These inline bytes are **not page-aligned and not buddy frames**, so they can
never be mapped into a user page table as shared pages. `memfd_create`
(`kernel/src/syscall.rs:6014`) is exactly "a tmpfs file": it `VFS_OPEN`s
`/tmp/memfd:<name>` and returns the fd. Nothing marks it seal-capable.

**Seals are faked.** `handle_fcntl`'s catch-all `_ => ok_reply()`
(`vfs/src/lib.rs:3161`) swallows `F_ADD_SEALS`/`F_GET_SEALS` with a success and
stores nothing. `handle_ftruncate` (`lib.rs:3834`) never consults seals.

**VFS can allocate/reach frames.** `servers/vfs` depends on `mm`
(`servers/vfs/Cargo.toml`) and already `extern crate alloc`s
(`vfs/src/lib.rs:36`), so it can call `mm::buddy::alloc/free`,
`mm::phys_to_virt`, `mm::pageref`, and hold a `Vec<usize>`.

**Acceptance test shape** (`userland/scmtest/src/main.rs`):
- subtest 3: `memfd_create` → `ftruncate(4096)` → parent `mmap(MAP_SHARED,4096)`
  → writes pattern A → passes fd via SCM_RIGHTS → child `mmap(MAP_SHARED)` on the
  received fd must see A (a mapping made **after** the writes), writes B →
  parent's **pre-existing** mapping must see B. Pure mmap loads/stores; no
  `read()`/`write()` on the memfd. Never shrinks.
- subtest 4: `ftruncate(4096)` → `F_ADD_SEALS(F_SEAL_SHRINK)`==0 →
  `F_GET_SEALS` reports it → `ftruncate(10)` must be `-1/EPERM`.
- SCM_RIGHTS fd passing itself is **blocker #1**, a *separate* K1 item
  (net-server cmsg plumbing). This design only requires that a passed fd resolve
  in the receiver to the *same tmpfs slot* — which is what fd-passing already
  means — so the VMO must be **keyed on the tmpfs owner slot**, never on the fd
  or on a per-open token.

---

## 1. Chosen design: eager shared VMO, keyed on the tmpfs owner slot

A **VMO** is a page list owned by a tmpfs inode (owner slot). Its frames are the
*single source of truth* for that file's bytes: `read`/`write`/`ftruncate`/
`fstat` operate on them, and `MAP_SHARED` mmap installs those *same* frames into
user page tables. Coherence with `read()`/`write()` is therefore automatic and
free — the pages **are** the file (true page-cache unification for VMO-backed
files), not a write-back hack. No coherence/write-back machinery is needed.

**Eager, not lazy, mapping.** Because the VMO frames already exist (ftruncate
allocated them) and are refcounted individually by `pageref`, `sys_mmap` installs
the whole mapped range's frames at map time (`pageref::inc` each). This is the
key simplification:

- **Zero changes to the page-fault handler, `unmap_range`, `clone_as`,
  `AddressSpace::drop`.** A shared mapping is just a lazy VMA with
  `map_flags = MAP_SHARED`, `file_cap = 0`, and `lazy_pages` **pre-populated**
  with the borrowed frames. Every existing path that walks `lazy_pages` +
  `pageref` already does the right thing (fork shares+incs, munmap/exit
  unref_or_frees, partial munmap trims).
- **No new hook type, no fault-into-VFS from fault context**, so no new
  fault-context re-entrancy or deadlock surface.
- **No "hook into a freed slot" hazard**: once mapped, the VMA holds real
  `pageref`'d frames; slot teardown only drops the VMO's own reference.

**Refcount model** (composes cleanly with the untracked-⇒-1 convention):
- VMO allocates a frame ⇒ leave it **untracked** (implicit refcount 1 = "the VMO
  owns it"). *The VMO never calls `pageref::inc` on allocation.*
- Each shared mapping that borrows the frame ⇒ `pageref::inc` (done in the VFS
  acquire call, under the VMO lock, see §4).
- Mapping released (munmap/partial/exit) ⇒ `unref_or_free` (existing code).
- fork of the mapping ⇒ `pageref::inc` (existing `clone_as` MAP_SHARED branch).
- VMO drops a frame (ftruncate-shrink, slot teardown) ⇒ `unref_or_free` (drops
  the implicit VMO reference; frees only if no mapping still holds it).
Every holder does exactly one acquire and one release; the frame is freed
exactly once by whoever is last. Worked example:
`alloc`(untracked=1) → map-acquire `inc`(2) → fork `inc`(3) → parent munmap
`unref`(2) → child exit `unref`(1, untracked) → slot teardown `unref`(free). ✔

**Which files get a VMO:** `memfd` files are VMO-backed from creation (required
for subtests 3+4, and this removes the 32 KiB inline cap for memfd — good for
real wl_shm pools). The *same mechanism* promotes a regular tmpfs file on its
first `MAP_SHARED` (covers `/dev/shm`, `/run/user/$UID` via `shm_open`); the
promotion migrates any existing inline `data[..len]` into frames. Files never
memfd'd and never `MAP_SHARED`-mapped keep the exact inline `data[]` path —
**so vfstest/coreutils tmpfs I/O is byte-for-byte unchanged (zero regression
risk on the hot path)**. tmpfs-`MAP_SHARED` promotion can be feature-gated/
deferred to a follow-on chunk if desired without affecting the memfd acceptance.

**Why not lazy + a new "give me the file's frame" fault hook?** It's viable but
strictly more code and more risk: a new hook type, fault-context entry into the
VFS/TMP_FILES lock, and a "map still lazy after the slot is freed" lifetime
question. Eager sidesteps all three. The only cost of eager is that mapping an
N-page region allocates N frames up front — which is exactly what a real
page-cache-backed shared file consumes once wl_shm fills the pool anyway.

---

## 2. Data-structure changes

**`servers/vfs/src/lib.rs` — new VMO store (parallel table, keyed by owner slot;
avoids bloating the `const`-init `TmpFileEntry`):**
```rust
struct TmpVmo {
    pages: alloc::vec::Vec<usize>, // phys frame per 4K page index; non-sparse
    len:   usize,                  // logical file size in bytes (== entry.len mirror)
    seals: u32,                    // F_SEAL_* bits
    is_memfd: bool,                // seals only permitted on memfd inodes
}
static TMP_VMOS: Mutex<[Option<TmpVmo>; MAX_TMP_FILES]> =
    Mutex::new([const { None }; MAX_TMP_FILES]);
```
Indexed by `tmp_owner(idx)` so hard links / passed fds share one VMO.
`pages` is kept **non-sparse** (every index in `0..ceil(len_or_mapped/4096)` is a
real frame) so the eager mmap and read/write never hit a hole.

**`mm/src/vmm.rs` — one new method (no struct changes):**
`AddressSpace::map_shared_frames(&mut self, virt, frames: &[usize], flags) -> bool`.
Creates a lazy VMA with `map_flags = MAP_SHARED`, `file_cap = 0`, `cow = false`,
`lazy_pages = frames.to_vec()`, `lazy_count = frames.len()`, and `map_page`s each
frame with `flags` (writable). Frames arrive **already `pageref::inc`'d** by the
VFS acquire call (§4); this method does not inc. On partial failure it unmaps
what it installed and `unref_or_free`s **all** passed frames, returns false.

No other type changes anywhere.

---

## 3. Syscall-by-syscall behavior deltas

**mmap** (`kernel/src/syscall.rs sys_mmap`, insert a branch after the device
check ~`:1470`, before the eager-copy file path):
- If `flags & MAP_SHARED != 0` **and** `vfs_get_node_kind(pid,fd)` is
  `VnodeKind::TmpFile{..}`: call `vfs::vmo_acquire_frames(pid, fd, off, len)`
  → `Option<Vec<usize>>` (frames pre-`inc`'d, VMO grown to cover `[off,off+len)`),
  then `with_current_address_space_mut(|as_| as_.map_shared_frames(virt,&frames,page_flags))`.
  Success ⇒ return `virt`; failure ⇒ have VFS release the frames + return `-ENOMEM`.
- Everything else (MAP_PRIVATE, f2fs MountedFile, device) ⇒ **unchanged** existing
  path. f2fs MAP_SHARED stays a private copy, as allowed.

**munmap** — unchanged. `unmap_range` (`vmm.rs:661`) already `unref_or_free`s each
`lazy_pages` frame in the clipped range and reshapes the VMA (full/front/back
trim). Partial munmap of a shared mapping drops exactly the trimmed frames'
refs. (The pre-existing "middle-split leaks the right eager portion" caveat
does not apply — shared VMAs are lazy.)

**fork/clone** — unchanged. `clone_as` (`cow.rs:184-209`) sees
`map_flags & MAP_SHARED`, `pageref::inc`s each present `lazy_pages` frame,
installs full-permission mappings in the child, leaves the parent writable,
sets neither side CoW. `file_cap == 0` ⇒ no device path, no file_retain. Child
and parent alias the same VMO frames.

**exit** — unchanged. `AddressSpace::drop` (`vmm.rs:148-164`) `unref_or_free`s
each `lazy_pages` frame; `file_cap == 0` ⇒ no file_release.

**ftruncate** (`vfs/src/lib.rs handle_ftruncate`, TmpFile arm):
- If owner has a VMO: **enforce seals** — if `new_len < vmo.len &&
  vmo.seals & F_SEAL_SHRINK != 0` ⇒ `EPERM`. Grow ⇒ append zeroed buddy frames
  to `pages` (untracked). Shrink ⇒ `unref_or_free` each dropped frame (drops the
  VMO's implicit ref; a frame still mapped survives), truncate `pages`. Set
  `vmo.len = new_len`; mirror `entry.len = new_len`. Removes the 32 KiB cap for
  VMO files.
- No VMO ⇒ existing inline path unchanged.

**fcntl** (`vfs/src/lib.rs handle_fcntl`, add two arms; the catch-all keeps
handling everything else):
- `F_ADD_SEALS (1033)`: resolve fd→owner VMO. `EINVAL` if no VMO or not
  `is_memfd`. Else `vmo.seals |= arg; return 0`. (Optionally honor an existing
  `F_SEAL_SEAL` by rejecting further adds — not needed for the test.)
- `F_GET_SEALS (1034)`: `EINVAL` if no VMO; else return `vmo.seals`.

**read** (`vfs/src/lib.rs handle_read`, TmpFile arm ~`:2349`):
- If owner has a VMO: copy `n = min(count, vmo.len - cur, 4096)` bytes out of the
  VMO frames via `mm::phys_to_virt(frame)+page_off` (mirrors the existing
  4 KiB-capped chunking) instead of `entry.data`. Advance the fd position as
  today.
- No VMO ⇒ unchanged. This is the read↔mmap coherence guarantee (wl_shm
  compositor `read()` path).

**write** (`vfs/src/lib.rs handle_write`, TmpFile arm ~`:2435`):
- If owner has a VMO: honor `F_SEAL_WRITE`/`F_SEAL_GROW` if you choose to add
  them (not required by the test); grow `pages` as needed for `cur+n`, copy into
  frames via HHDM, bump `vmo.len`. **Not** capped at `MAX_TMP_SIZE`.
- No VMO ⇒ unchanged.

**fstat/stat** (`vfs/src/lib.rs:5117` TmpFile arm; and `stat_common`): report
`vmo.len` when a VMO exists, else `entry.len`.

**memfd_create** (`kernel/src/syscall.rs:6014`): after the `VFS_OPEN` returns the
fd, call `vfs::mark_memfd(pid, fd)` (creates an empty `is_memfd` VMO on the owner
slot). Change the open mode `O_WRONLY`→`O_RDWR` (`0x041`→`0x042`) so the
compositor-side `read()` coherence path is legal on the same fd (mmap loads
don't need it, but read() does).

**Slot teardown**: at the existing point where a tmpfs owner slot's bytes are
reclaimed (the `handle_close`/`handle_unlink` last-reference path,
`vfs/src/lib.rs:2550`/`4005`), if `TMP_VMOS[owner]` is set, `unref_or_free` each
frame and clear the entry. Frames still held by a live mapping survive (their
`pageref` > 1). *Note:* named tmpfs entries (incl. `/tmp/memfd:*`) already
persist by path until unlink — matching that existing lifetime is fine; even if
teardown is imperfect, live mappings keep frames alive via `pageref`, so there
is **no use-after-free**. Precise teardown is a correctness-neutral follow-on.

---

## 4. Locking analysis (invariant from 82d0cc3)

Invariant: **user memory must never be touched under a `RUN_QUEUE`/IRQ-off
spinlock.** This design never approaches that line:

- `map_shared_frames` runs under the **per-AS `busy` flag**
  (`with_current_address_space_mut`), *not* `RUN_QUEUE` — the same discipline
  every existing mmap/fork path uses (`vmm.rs:139` doc,
  `sched::lock_leader_address_space`). It touches page tables, `pageref` (a leaf
  `spin::Mutex`), and `map_page`; it does **not** dereference user pointers (the
  userspace stores happen later, in user mode).
- `vfs::vmo_acquire_frames`/`mark_memfd`/read/write/ftruncate/fcntl run in the
  caller's context under `TMP_FILES.lock()` + `TMP_VMOS.lock()` — ordinary
  leaf `spin::Mutex`es, never `RUN_QUEUE`, never IRQ-off. Frame zeroing touches
  **HHDM (kernel)** memory, not user memory. read/write copy to/from a user
  buffer that the kernel dispatch already **prefaulted** before calling
  `vfs::handle` (`syscall.rs:166 prefault_user`), so no fault is taken while the
  VMO/TMP_FILES lock is held — identical to the existing inline path's exposure.

**Lock ordering (no new AB-BA):**
- mmap acquires VMO frames **first** (lock `TMP_VMOS`→`TMP_FILES`, `inc`, unlock),
  **then** takes AS-`busy` to map. TMP_FILES is never nested inside AS-`busy`.
- The `inc` must happen *inside* the VMO lock in `vmo_acquire_frames` so a
  concurrent `ftruncate`-shrink's `unref_or_free` cannot free a frame between
  "listed" and "pinned" (pin-before-publish). `map_shared_frames` therefore does
  **not** re-inc; the VMA owns the transferred +1, released by munmap/exit. This
  splits the inc (VFS) from the dec (mm) across the boundary but is refcount-
  symmetric per VMA and race-free.
- `pageref` and `buddy` are leaf locks under both AS-`busy` and TMP_VMOS; no
  cycle.
- fork's `COW_LOCK` (`cow.rs:39`) is unaffected — shared VMAs take the existing
  `MAP_SHARED` clone branch which already runs under it.

---

## 5. Out of scope (explicit)

- **SCM_RIGHTS cmsg plumbing** (blocker #1, net-server
  `handle_sendmsg`/`handle_recvmsg`) — separate K1 item. This design only relies
  on a passed fd resolving to the same tmpfs owner slot, which fd-passing
  already guarantees. scmtest subtest 3 needs *both* items to pass.
- **f2fs `MAP_SHARED`** — stays a private copy (existing eager-copy path).
- **Mount of `/dev/shm` and `/run/user/$UID` (tmpfs)** — follow-on chunk. The
  VMO mechanism is path-agnostic (keyed on the owner slot), so `shm_open` on
  those mounts gets sharing "for free" via the first-`MAP_SHARED` promotion once
  the mounts exist.
- **Sparse/`SIGBUS`-on-hole semantics** and **ftruncate-shrink actively
  unmapping live mappings** (Linux sends SIGBUS) — minimal behavior keeps a
  still-mapped shrunk page valid (no UAF, no SIGBUS). Not exercised by scmtest
  (its shrink is seal-blocked).
- **`F_SEAL_WRITE` / `F_SEAL_GROW` / `F_SEAL_SEAL`** enforcement — only
  `F_SEAL_SHRINK` is required; the others are one-line additions later.
- **msync/write-back to a backing store** — unnecessary; VMO frames *are* the
  file. Nothing to flush.
- **Precise VMO teardown timing** — correctness-neutral (pageref prevents UAF);
  tighten later.

---

## 6. Diff size + riskiest spots

Estimated net **~250–300 lines** across 3 files:
- `mm/src/vmm.rs`: +~55 (one self-contained method; no edits to existing fns).
- `kernel/src/syscall.rs`: +~50 (mmap branch; memfd_create tweak).
- `servers/vfs/src/lib.rs`: +~150 (VMO struct+table, `vmo_acquire_frames`,
  `mark_memfd`, read/write/ftruncate/fstat/fcntl gated branches, teardown).

**Riskiest spots:**
1. **`pageref` inc/dec balance across the VFS↔mm boundary.** The VFS acquire
   incs; the mm VMA's munmap/exit/fork decs. An off-by-one either leaks frames
   or double-frees. Mitigation: single rule — "acquire incs once per VMA, VMA
   release decs once"; test fork+partial-munmap+exit combinations (see §7).
2. **Regressing tmpfs I/O (vfstest/coreutils).** Mitigated structurally: the
   VMO branch only activates when `TMP_VMOS[owner]` is `Some`; every non-memfd,
   non-`MAP_SHARED` file keeps the exact inline `data[]` code. Keep the branch a
   clean `if let Some(vmo) = ... { new } else { old }`.
3. **ftruncate-shrink freeing a frame a mapping still holds.** Must use
   `unref_or_free` (not `buddy::free`) so a mapped frame survives. Never
   exercised by scmtest but easy to get wrong.
4. **memfd fd access mode.** Leaving it `O_WRONLY` blocks the future read()
   coherence path; the mmap-only test passes either way, so this is a silent
   trap — switch to `O_RDWR`.
5. **VMO growth vs. logical len.** mmap may need frames past `vmo.len`
   (mapping larger than file). Keep `pages.len()` (frames) decoupled from
   `vmo.len` (EOF for read); read() must bound on `len`, mmap on frame count.

---

## 7. Test plan beyond scmtest

- **scmtest 3+4** on both arches (the acceptance gate). Note 3 also needs
  blocker-#1 SCM_RIGHTS; if that lands separately, unit-check the VMO half by a
  single-process variant: `memfd+ftruncate+two mmaps of the same fd`, write via
  one, read via the other (aliasing without fd-passing).
- **read↔mmap coherence** (the wl_shm requirement, *not* covered by scmtest):
  `memfd → ftruncate → mmap(MAP_SHARED) → store via mmap → read(fd)` sees the
  store; and `write(fd) → load via mmap` sees the write. Add this to scmtest or a
  sibling.
- **Refcount lifecycle matrix** (guards risk #1/#3), all on a shared memfd
  mapping: (a) fork, child writes, parent sees it, both exit cleanly (no leak/
  double-free); (b) partial munmap of the middle/ends, remainder still coherent;
  (c) `close(fd)` while mapped, mapping still readable/writable, then munmap;
  (d) ftruncate-grow then access new region through a re-map; ftruncate-shrink
  (unsealed) then the freed tail is reclaimed only after unmap.
- **Seals**: `F_SEAL_SHRINK` blocks shrink (EPERM) but allows grow and same-size
  ftruncate; `F_GET_SEALS` round-trips; `F_ADD_SEALS` on a non-memfd fd ⇒ EINVAL.
- **Regression**: full vfstest, polltest, forktest, memtest green both arches
  (per CLAUDE.md: release builds, `run-qemu.sh aarch64` + `x86_64`). Spot-check a
  coreutils run in `/tmp` (exercises the untouched inline path) for no size/
  content drift.
- **Stress/leak**: repeated map/unmap/fork cycles on a memfd; watch buddy free
  count returns to baseline (no frame leak), and a large (>32 KiB, e.g. 8 MiB)
  memfd mmap succeeds — proving the inline-cap removal for VMO files.
