# K1 — Shared file-backed mmap via a real VMO / page-cache layer

Design for LeandrOS (`/Users/forain/code/leandros`). Read-only survey; no code changed.

## 0. The one fact that reshapes the whole design

`servers/vfs` is **not** a separate process. `kernel/Cargo.toml:26` pulls it in as
`vfs-server`, and `syscall.rs` calls `vfs::handle(&msg, pid)` as a plain function
(`kernel/src/syscall.rs:30,1501,1540,…`). Both the kernel VMM (`mm`) and the VFS
server link the **same `mm` crate** (`servers/vfs/Cargo.toml` deps `mm`). So the
"kernel ↔ vfs server" boundary is a *module* boundary inside one address space,
not an IPC/address-space boundary. Pages never have to be marshaled. The
"where do pages live / who owns them" question therefore collapses to: *pick the
crate that both sides already depend on* — `mm`.

Second fact: tmpfs bytes today live **inline** in a fixed array
`TmpFileEntry::data: [u8; 32768]` inside `static TMP_FILES: Mutex<[TmpFileEntry;128]>`
(`servers/vfs/src/lib.rs:263,311`). They are not page-aligned and not in buddy
frames. read/write `copy_nonoverlapping` straight in/out of `entry.data`
(`:2358`, `:2452`). For an mmap to alias the *same physical pages* that read/write
touch, the bytes have to move into page-aligned buddy frames — that is the VMO.

Third fact: the refcount substrate already exists. `mm::pageref`
(`mm/src/pageref.rs`) is a global `phys → u32` map; "absent = 1". `unmap_range`,
`AddressSpace::drop`, and `clone_as` already call `pageref::inc` /
`unref_or_free` per page (`mm/src/vmm.rs:688,154`; `mm/src/cow.rs:170,238`).
A VMO frame is just a frame that carries **one extra pageref for the VMO itself**
on top of the per-PTE refs. This unifies VMO teardown with the existing CoW
lifecycle — no parallel refcount scheme.

---

## 1. The VMO abstraction

New module `mm/src/vmo.rs`, registered in `mm/src/lib.rs`. `mm` is the common
dependency of kernel + vfs (+ f2fs, drm later), so it is the correct home.

```rust
// mm/src/vmo.rs
pub struct VmoId(u64);              // opaque handle, 0 == none

struct Vmo {
    /// Page frames. frames[i] = phys of logical page i (always present/eager
    /// for now; 0 reserved for a future lazy hole). Non-contiguous.
    frames: alloc::vec::Vec<usize>,
    /// Logical byte length (the file size the VMO stands for).
    len: u64,
}

static VMOS: spin::Mutex<BTreeMap<u64, Vmo>> = ...;   // registry; leaf lock
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

// ── API used by vfs (policy) ──────────────────────────────────────────────
pub fn create(len: u64) -> Option<VmoId>;   // alloc ceil(len/4K) frames,
                                             // zero them, pageref-owned (=1)
pub fn resize(id, new_len) -> bool;          // grow: alloc+zero+own new frames;
                                             // shrink: unref_or_free tail frames
pub fn read (id, off: u64, dst: *mut u8, n)  -> usize;  // HHDM copy frame→dst
pub fn write(id, off: u64, src: *const u8,n) -> usize;  // HHDM copy src→frame,
                                                        //   grows if needed
pub fn len(id) -> u64;
pub fn release(id);          // drop the VMO's own ref on every frame
                             //   (unref_or_free each), remove from registry

// ── API used by the kernel mmap path (mechanism) ──────────────────────────
pub fn snapshot_frames(id, off: u64, len) -> Vec<usize>; // phys list for a
                                                         //   mapping window
```

Frames are copied to/from via `mm::phys_to_virt` (HHDM). HHDM is always mapped
and never faults — so **no VMO operation ever touches user memory** (important
for §4).

### Lifecycle / ownership boundary

| Layer | Owns | Responsibility |
|---|---|---|
| `mm::vmo` | the frames + registry | allocate/free frames, refcounts, HHDM copies, hand out phys lists |
| `servers/vfs` | inode → `VmoId` binding | *decide when to promote*, route read/write/ftruncate/size through the VMO once promoted, `release()` on inode teardown |
| kernel `sys_mmap` | address-space wiring | ask vfs for the `VmoId`, install its frames as a shared VMA |

Refcount invariant (the whole correctness argument in one line):
**every frame carries exactly one pageref for the VMO plus one per mapping PTE;
the frame is freed only when both are gone.** VMO `create/resize` establish the
+1; each PTE install `pageref::inc`s; every teardown path (`munmap`,
`unmap_range` trim, `AddressSpace::drop`, `resize` shrink, `release`) calls
`unref_or_free`. No new counter, no new free path.

### tmpfs inode states (blast-radius control)

A tmpfs inode is in one of two states:

* **Inline** (default, unchanged): bytes in `entry.data`, `entry.vmo == None`.
  Every existing tmpfs file, every vfstest path, stays exactly here. Zero risk.
* **VMO-backed**: on the **first `MAP_SHARED` mmap**, the inode is *promoted*:
  `vmo::create(entry.len)`, copy `entry.data[..len]` into the VMO, store
  `entry.vmo = Some(id)`. From then on **all** data access on that inode
  (read/write/ftruncate/stat-size) routes through the VMO. `entry.data` is dead
  for that inode.

Promotion is keyed on the **owner slot** (`tmp_owner(idx)`, `:1392`), i.e. the
inode, so every fd/hardlink/SCM-passed alias resolves to the same VMO. This is
what makes two processes' mappings alias.

Why lazy promotion instead of "all tmpfs is VMO": it keeps the common path
byte-for-byte unchanged (regression containment) and pays frame allocation only
for files that are actually shared-mapped (memfds/wl_shm pools).

---

## 2. Exact touch list (file : function)

**mm (mechanism)**
* `mm/src/lib.rs` — `pub mod vmo;`  (+2 lines)
* `mm/src/vmo.rs` — **new**, the registry above  (~160 lines)
* `mm/src/vmm.rs` — `AddressSpace::map_shared_frames(virt, &[phys], flags)`:
  install a *pre-populated shared* VMA (see §3 mmap). Reuses the existing
  `lazy_pages` representation. (~45 lines)
* `mm/src/vmm.rs::VmaRegion` (`:33`) — **optional** `vmo_id: u64` field, forward-compat
  only (writeback/dmabuf backmapping). Not needed for correctness; costs ~6
  struct-literal edits. See §5.

**kernel**
* `kernel/src/syscall.rs::sys_mmap` (`:1371`, MAP_SHARED branch replacing the
  degrade comment at `:1436`)  (~50 lines)
* `kernel/src/syscall.rs::sys_memfd_create` (`:6015`) — open **O_RDWR** not
  O_WRONLY (`:6033`, `0x041`→`0x042`) so a PROT_WRITE MAP_SHARED is legitimate. (1 line)

**vfs (policy)**
* `TmpFileEntry` (`:260`) — add `vmo: Option<u64>` and `seals: u32`. (+2 fields,
  `empty()` at `:301`)
* new `fn tmp_promote_vmo(entry) -> u64` and `fn tmp_free_inode(entry)`
  (release VMO then clear)  (~40 lines)
* `handle_read` TmpFile arm (`:2349`) — if `owner.vmo` set, `vmo::read` instead
  of `entry.data`
* `handle_write` TmpFile arm (`:2435`) — if set, `vmo::write` (grows VMO)
* `handle_ftruncate` (`:3834`) — seal check (below) + `vmo::resize` when VMO-backed
* `handle_fcntl` (`:3098`) — real `F_ADD_SEALS`/`F_GET_SEALS` replacing the
  `_ => ok_reply()` catch-all (`:3161`)  (~25 lines)
* `tmp_drop_name` (`:1431`, the 3 `= TmpFileEntry::empty()` sites) — route inode
  frees through `tmp_free_inode`
* new `pub fn vfs_get_shared_vmo(pid, fd, want_write) -> Option<(u64 /*id*/, u64 /*len*/)>`
  next to `vfs_get_node_kind` (`:417`) — resolves fd→owner, promotes on demand,
  returns id+len. (~30 lines)

Est. total ≈ **420 LOC**.

---

## 3. Syscall-by-syscall deltas

### mmap (`sys_mmap`, MAP_SHARED + file fd)
Insert a branch *before* the existing copy-on-map file path:
```
if (flags & MAP_SHARED) && !(flags & MAP_ANONYMOUS):
    let (id, flen) = vfs::vfs_get_shared_vmo(pid, fd, prot&PROT_WRITE)?  // else fall through / ENODEV
    let frames = mm::vmo::snapshot_frames(id, off, len)                 // phys list, VMO holds refs so stable
    with_current_address_space_mut(|as_| {
        if MAP_FIXED { as_.unmap_range(virt, len) }
        as_.map_shared_frames(virt, &frames, page_flags)                // installs PTEs, pageref::inc each
    })
    return virt
```
`map_shared_frames` builds a VMA that is **`lazy=true` with `lazy_pages`
pre-filled and every PTE already installed**, `map_flags |= MAP_SHARED`,
`cow=false`, `file_cap=0`. This is *deliberately shaped identical to an anonymous
MAP_SHARED region whose pages are all faulted-in* — so it inherits, unchanged,
the fork/munmap/exit/mprotect machinery that already handles anon MAP_SHARED.
(`file_cap` stays 0 so the exec-file retain/release hooks at `:110/117` are NOT
invoked — those are for the ELF loader, wrong owner here.)

Coherency (subtest 3): parent mmaps first → promotes inode, copies existing
(zeroed) bytes into VMO, maps VMO frames. Parent's `*p = A` store lands in a VMO
frame. Child mmaps same inode → already VMO-backed → maps the *same* frames →
reads A, writes B into the same frames → parent's pre-existing mapping reads B.
read()/write() coherency: both route through the same frames.

### munmap (`sys_unmap_mem`/`unmap_range`, `:1572`/`:661`)
No change. The shared VMA is `lazy` → existing lazy branch `unref_or_free`s each
frame (`:688`). Frame survives because the VMO still holds its ref. Partial and
front/back-trim already handled (`:711-728`). "Map before peer maps / close in any
order" all fall out of pageref.

### fork (`clone_as`, `mm/src/cow.rs:48`)
No change. The VMA is `lazy` + `is_shared` + all frames present → hits the
"already per-page tracked" branch (`:184`): `pageref::inc` each frame, install
into child with **full flags** (`is_shared ⇒ region.flags`, `:193`), parent left
writable, `cow=false`. Child shares the same frames R/W. (The latent anon-shared
"unfaulted page diverges across fork" gap at `:187` `if phys==0 continue` does not
bite us — VMO shared VMAs are eager, so no frame is absent at fork time.)

### exit (`AddressSpace::drop`, `mm/src/vmm.rs:142`)
No change. Lazy branch `unref_or_free`s each frame (`:154`); VMO ref keeps live
frames alive for surviving peers; last ref frees.

### ftruncate (`handle_ftruncate`, `:3834`)
```
if entry.seals & F_SEAL_SHRINK && new_len < entry.len: return EPERM   // subtest 4
if entry.vmo.is_some(): mm::vmo::resize(id, new_len)                  // grow: +frames; shrink: unref tail
else: (existing inline path)
entry.len = new_len
```
Shrink with a live mapping: `vmo::resize` only drops the *VMO's* ref on tail
frames; a mapping's PTE ref keeps them alive → no use-after-free (we don't model
SIGBUS on the truncated region — documented simplification). Grow does not
extend existing mappings (matches Linux; the mapping length was fixed at map time).

### fcntl (`handle_fcntl`, `:3098`; kill the `_ => ok_reply()` at `:3161`)
```
F_ADD_SEALS (1033): entry(owner).seals |= arg;  ok_reply()
F_GET_SEALS (1034): val_reply(entry(owner).seals)
_ => ok_reply()   // keep for the genuinely-ignored rest
```
Stored on the owner slot. Enforced in ftruncate (F_SEAL_SHRINK now; F_SEAL_GROW/
WRITE are cheap follow-ons). This subtest needs **neither** SCM_RIGHTS nor the
VMO — it can land and pass first, independently.

### read / write (`handle_read` `:2349`, `handle_write` `:2435`)
When `owner.vmo` is set, swap the data source: `mm::vmo::read(id, pos, buf, n)` /
`mm::vmo::write(id, pos, buf, n)` (write grows the VMO, capped at MAX_TMP_SIZE for
now). Everything else — the 4 KiB cap, pos write-back dance — unchanged. This is
what makes read()/write() coherent with mmap stores (hard requirement for wl_shm
compositors that read() the pool).

---

## 4. Locking analysis (invariant: never touch user memory under RUN_QUEUE / IRQ-off spinlocks — deadlock class from 82d0cc3)

* The MAP_SHARED install runs under the **per-AS `busy` flag**
  (`with_current_address_space_mut` → `lock_leader_address_space`,
  `sched/src/lib.rs:1052,1695`), **never RUN_QUEUE**. Same discipline the fault
  handler already uses (`handle_page_fault:1085`).
* Lock order (all leaf, acyclic):
  `AS.busy → FD_TABLES → TMP_FILES → VMOS → PAGE_REFS`.
  `vfs_get_shared_vmo` takes FD_TABLES→TMP_FILES→VMOS then returns; the kernel
  then takes `busy` and installs PTEs (VMOS snapshot already released; frames
  pinned by the VMO ref). `COW_LOCK` is on a separate axis
  (`busy → COW_LOCK → PAGE_REFS`, `cow.rs:39`); VMO ops never take it.
* **No VMO/vfs operation dereferences a user pointer.** Frame fills are HHDM
  (`phys_to_virt`) copies; PTE installs touch page-table memory + buddy nodes.
  HHDM and page tables are always mapped → cannot fault → cannot re-enter
  `lock_leader_address_space`. So the self-deadlock the signal path warns about
  (`sched/src/signal.rs:363-366`) is structurally impossible here.
* `pageref::inc` at map time vs a concurrent `clone_as` inc: both are plain
  increments under the `PAGE_REFS` mutex — safe. The compound get→copy→dec that
  `COW_LOCK` guards never runs on VMO frames (`cow=false`, never promoted).
* `TMP_FILES` is held across the `vmo::create` copy during promotion (HHDM only,
  bounded, no re-entrancy into vfs) — acceptable; matches existing
  `TMP_FILES`-held copies in read/write.

---

## 5. Forward-compatibility hooks — bought vs deferred

| Hook | Bought now? | Cost now | Buys later |
|---|---|---|---|
| VMO object lives in **`mm`** (shared crate), not vfs | **Yes** | ~150 LOC registry vs a Vec-on-inode | f2fs page cache & drm/dmabuf reuse the *same* substrate instead of each growing its own frame store |
| pageref-unified frame lifecycle | **Yes** | ~0 (reuse) | dmabuf/GEM sharing, MAP_SHARED-on-real-file all teardown-correct for free |
| `snapshot_frames(id, off, len)` window API | **Yes** | trivial | non-zero mmap offsets, partial-file maps, sub-buffer dmabuf |
| `VmaRegion.vmo_id` back-pointer | **Optional** | ~6 literal edits | MAP_SHARED **writeback** (dirty page→file), lazy shared fault-fill; retrofitting later re-touches the same literals, so pay it now if writeback is on the near roadmap |
| Lazy (hole) VMO frames + shared fault-fill | **No** — eager only | 0 | sparse huge memfds; add a `frames[i]==0` fault case calling `vmo::fault_get_frame` later |
| f2fs MAP_SHARED | **No** (stays degraded, allowed) | 0 | — |
| Dirty/writeback tracking, SIGBUS on truncated region | **No** | 0 | POSIX-exact truncate/msync semantics |

The eager-frames choice is what lets the mapping reuse the anon-shared VMA shape
and touch **zero** lines of fork/munmap/exit — the single biggest risk reducer.

---

## 6. Diff size + riskiest spots

≈ **420 LOC** (mm ~210, kernel ~55, vfs ~155).

Riskiest, in order:
1. **Refcount off-by-one.** One extra free → buddy corruption surfacing as a
   *later, unrelated* alloc failure; one missing free → slow frame leak. Mitigation:
   the single invariant in §1 (VMO = +1 ref; PTE = +1 ref) and routing *every*
   inode-free through `tmp_free_inode`.
2. **Stale `entry.data` reads after promotion.** Any code path still reading
   `entry.data`/`entry.len` directly on a promoted inode returns zeros. Must audit
   *every* `entry.data`/`entry.len` use in `servers/vfs/src/lib.rs` (read, write,
   ftruncate, fstat size, getdents ignores it, sendfile if any) and gate on
   `entry.vmo`.
3. **Inode-free release sites.** `tmp_drop_name` has 3 `= TmpFileEntry::empty()`
   sites (`:1437,1439,1446`); all must go through `tmp_free_inode` or the VMO leaks.
4. **memfd O_WRONLY** (`:6033`) — without the O_RDWR fix a strict PROT_WRITE check
   would reject the test's mmap; also read()-coherency on a memfd needs read access.

---

## 7. Test plan (beyond scmtest)

Regression (must stay green, both arches): `vfstest`, `polltest`, `forktest`,
`memtest`. vfstest is the key witness that the inline path is untouched.

New / focused (add as a small userland test or extend scmtest):
* **Single-process coherency both directions**: memfd+ftruncate+mmap MAP_SHARED;
  write via mmap → `read()` sees it; `write()` via fd → mmap load sees it.
* **fork sharing**: parent maps, forks, child writes, parent sees; and reverse.
* **Teardown orders**: parent-munmap-first vs child-first vs process-exit-without-
  munmap; loop many iterations so a double-free/leak shows as buddy exhaustion or
  a corrupted later allocation.
* **ftruncate vs live mapping**: grow-then-access-new-region (own mapping stops at
  old len — expected), shrink-then-access (no kernel fault/UAF), partial munmap of
  a shared region.
* **Seals**: F_ADD_SEALS/F_GET_SEALS round-trip; ftruncate-shrink after
  F_SEAL_SHRINK → EPERM (subtest 4, standalone).

Cross-dependency to flag to the orchestrator: **subtest 3 also needs K1 blocker #1
(SCM_RIGHTS)** so the child's received fd resolves to the same tmpfs owner idx —
that is net-server/vfs work, *not* in this VMO change. Subtest 4 (seals) and the
single-process coherency test pass on the VMO/vfs work **alone**.

---

## 8. Honest comparison — minimal reuse-anon vs this VMO design

**Minimal approach.** Skip `mm::vmo` entirely. On first MAP_SHARED, tmpfs
allocates a `Vec<phys>` of frames *on `TmpFileEntry`*, copies `data` in, routes
read/write through it, and exposes `vfs_get_shared_frames(fd) -> Vec<phys>`; the
kernel installs them with the exact same pre-filled-shared-lazy-VMA trick
(`map_shared_frames`). It passes subtests 3 + 4, reuses all anon-shared
fork/munmap/exit machinery, and lands at **~250 LOC** — ~170 less than this
design. The *only* structural difference: frame storage + lifecycle live in vfs
instead of mm.

**Where the minimal one lands short**, and why this design's extra ~170 LOC is worth it:
* **f2fs page cache** (post-K1): f2fs is a *different* server; a Vec-on-tmpfs-inode
  can't be reused, so f2fs would grow a second, parallel frame store.
* **M7 graphics wave (dmabuf/GEM)**: a dmabuf *is* a VMO shared between drm-server
  and clients — the framing this task explicitly calls out. The mm registry is its
  natural home; frames-in-vfs cannot serve drm.
* **MAP_SHARED writeback / dirty tracking**: needs a page→object backmap that lives
  with the object, i.e. the VMO.
* **One place to audit refcounts** when the sharing graph gets non-trivial.

Decision rule: if the roadmap were "wl_shm and nothing after," take the minimal
approach. Because K1 is explicitly the substrate for the M7 graphics wave and
future f2fs/writeback, pay the ~170 LOC for the mm-resident VMO now — retrofitting
a registry after three servers have each grown their own frame Vec is a larger,
riskier change than building it once. Both designs share the *same* kernel-side
mmap/fork/munmap surface (`map_shared_frames` + anon-shared reuse), so the choice
is reversible on the vfs side alone if the roadmap changes.
