# f2fs runtime-mkdir `?---------` corruption — static root cause

HOST-ONLY read-only analysis. Repo untouched. Sources:
- Kernel/server: `/Users/forain/code/leandros/servers/f2fs/src/lib.rs`
- mkfs: `/Users/forain/code/leandros/scripts/mkfs-f2fs-populated.py`
- Observations: `~/code/leandros-artifacts/notes/m6-progress.md` (BUG B lines 74-77; ?-type lines 103-118, 955-960, 983-993, 1031-1033)

---

## VERDICT (up front)

The `?---------` broken-directory corruption is **a crash-consistency gap, not a
mkdir format bug.** The kernel's on-disk dentry/inode/NAT format is
self-consistent and matches mkfs exactly; a mkdir performed within a single boot
always produces a valid directory. The corruption appears only after the VM is
**hard-killed with no clean unmount** (exactly what the M6/driver.py workflow
does), because directory-namespace mutations are **not checkpointed
synchronously** and the 4-slot writeback block cache evicts *some* of a mkdir's
blocks to disk out-of-band while the matching metadata stays in RAM and is lost.

Confidence: crash-consistency = **HIGH**. A separate genuine within-boot mkdir
bug (BUG B "deepest level absent") = **LOW** — almost certainly the same
crash/pollution confound, or a kernel-VFS dcache artifact outside f2fs, not an
f2fs on-disk defect.

---

## 1. The on-disk formats agree (no mkfs-vs-kernel mismatch, no self-inconsistency)

Volume is built `^extra_attr,^inline_data,^inline_dentry` (lib.rs:8), so every
inode uses `INODE_UNION = 364` as the block-pointer base and every directory
uses regular 4 KB dentry blocks. Cross-checked field by field:

**Dentry block** — identical constants both sides:
- kernel: `NR_DENTRY_IN_BLK=214`, `DENTRY_BITMAP_SIZE=27`, `DENTRY_RESERVED=3`,
  `DENTRY_ENTRIES_OFF=30`, `DENTRY_ENTRY_SIZE=11`, `DENTRY_NAMES_OFF=2384`,
  `DENTRY_SLOT_LEN=8` (lib.rs:160-166).
- mkfs: same values (`NR_DENTRY_IN_BLK=214`, `DENTRY_NAMES_OFF=2384`, slot len 8),
  same multi-slot layout — `slots_used = ceil(name_len/8)`, bitmap bit per slot,
  entry at `ENTRIES_OFF + slot*11`, name at `NAMES_OFF + slot*8`
  (mkfs 88-131). The kernel writer (`dir_add_entry`, lib.rs:1522-1595) and reader
  (`dir_lookup_ft`, lib.rs:1479-1519) use the **same** slot arithmetic and the
  same multi-slot skip (`slots_used = (name_len+7)/8`). Bitmap bit ↔ slot ↔ name
  offset are aligned on all three code paths.

**Inode block** — consistent:
- mkfs `build_inode_block` (mkfs 133-147): mode@0, i_advise@2=0, **i_inline@3=0**,
  links@12, size@16, block addrs @364+idx*4, footer nid@4076, ino@4080.
- kernel `create_inode` (lib.rs:747-776): mode@0, uid@4, gid@8, links@12=1,
  size@16=0, i_pino@84, footer nid@4076, ino@4080. Byte 3 (i_inline) left 0 by
  the zero-init buffer.
- `inode_addr_base` (lib.rs:202-210) keys the pointer base on
  `i_inline & F2FS_EXTRA_ATTR(0x20)`. Both sides leave byte 3 = 0 ⇒ base = 364 on
  both ⇒ block pointers read back correctly. **No extra_attr/inline divergence.**

**NAT / next_free_nid** — consistent:
- mkfs `write_nat_entry` (mkfs 173-179) and kernel `nat_update`/`nat_lookup`
  (lib.rs:502-518) use the identical in-place formula
  `nat_blkaddr + ino/455`, offset `idx*9`, version@+0 / ino@+1 / blkaddr@+5.
  Neither consults the f2fs `nat_bitmap` (single-copy NAT by convention). They agree.
- mkfs sets `next_nid = max(dir_ino)+1` then bumps per packed file/dnode
  (mkfs 749, 802-916); checkpoint stores it at CP+152 (mkfs 171). Kernel reads it
  at mount and hands out `ms.cp.next_free_nid++` in `create_inode`/`create_node_block`
  (lib.rs:749-750, 1455-1456). Runtime nids never overlap mkfs nids **on a fresh
  image**. (This becomes false after a stale-checkpoint reboot — see §3.)

**Runtime allocator vs mkfs layout** — safe:
- mkfs marks its used segments' SIT vblocks and points the runtime node/data logs
  at the first *fully-empty* segments past all mkfs data, in **distinct** segments
  (`cur_node_segno=used_segs`, `cur_data_segno=used_segs+1`, mkfs 1000-1044). The
  append-only allocator (`alloc_node_block`/`alloc_data_block`, lib.rs:655-683)
  never overwrites mkfs data for the first segment's worth of allocations.

**Minor real defects found, none of which cause `?-type`:**
- New directory gets `i_links = 1` (lib.rs:761) instead of 2, and the parent's
  link count is **not** incremented for the child's `..` (handle_mkdir never
  touches parent nlink). mkfs, by contrast, writes `links = 2 + n_subdirs`
  (mkfs 990). Cosmetic/fsck-visible; does not affect type bits or lookup.
- `create_inode` omits the footer word mkfs writes at `NODE_FOOTER_OFF+16 = 1`
  (mkfs 146). The kernel never reads it. Harmless.

Because reader and writer share every constant and the cache is coherent
**within a boot** (see §2), a directory created at runtime and looked up later in
the *same* boot is always found with the correct `DT_DIR`/`S_IFDIR`. This is
confirmed by the notes' own clean-image success (m6-progress:114 comp2-home,
"`/root/.config` = proper `drwxr-xr-x`").

---

## 2. Why the corruption is crash-consistency, mechanically

### 2a. Within a boot the cache is coherent — so mkdir itself is correct
`BlockCache` (lib.rs:274-359) is a 4-slot write-back cache. `find()` guarantees a
block occupies at most one slot; `read`/`write`/`get_mut` all flush a dirty
victim before reuse. The f2fs server is single-threaded and finishes each request
before the next. Therefore between requests the in-RAM state (cache + `ms.cp`) is
always internally consistent, and any read sees the latest write. A within-boot
`mkdir -p a/b/c` cannot lose the deepest level from cache incoherence.

### 2b. There is no synchronous checkpoint on a namespace mutation
Durability is provided only by `flush_checkpoint` (lib.rs:687-715), which does
`cache.flush_all` → write CP block → `virtio_blk::flush`. It is reached from just
two places:
- `handle_fsync` (lib.rs:725-728) — explicit fsync, and
- `maybe_flush` (lib.rs:730-735) — **only after 16 accumulated dirty writes.**

`handle_mkdir` (lib.rs:2166-2208) never calls `flush_checkpoint` directly; it
only inherits the 3 `maybe_flush` ticks from its three `dir_add_entry` calls
(parent, `.`, `..`). So a checkpoint lands roughly every **5-6 mkdirs**, at an
arbitrary boundary unrelated to any single operation. `handle_open` O_CREAT
(lib.rs:~1893) and every other namespace mutator (symlink/link/rename/unlink/
rmdir, the `maybe_flush` sites at 2325/2425/2509/2566/2638/2686/2711) have the
same every-16 behavior. `unmount()` checkpoints (lib.rs:3305-3312) — but only on
a *clean* shutdown.

### 2c. The writeback cache tears the operation across the crash
Between checkpoints the 4-slot cache evicts dirty blocks to disk independently
and in LRU order (lib.rs:288-294, 303-309). A single mkdir dirties several
distinct blocks: the child inode block (node log), the child's new dentry data
block, the **parent's** dentry data block, the shared **NAT** block, and SIT
blocks — easily more than 4 live blocks, so evictions happen mid-operation.

At a hard kill (QEMU SIGKILL / HVF teardown — no `unmount()`), the disk equals
*(last checkpoint's CP + NAT + SIT + curseg pointers)* plus *(whatever arbitrary
subset of dirty blocks LRU happened to have evicted since)*. That subset is not a
consistent snapshot. The `?---------` inode is the direct signature of the tear:

> the **parent dentry block was evicted** (now names child → nid N on disk) but
> the **NAT block was not** (no entry for N) — or the child **inode block was not**
> written. On reboot the mount reads the stale checkpoint: `dir_lookup` finds the
> name, `nat_lookup(N)` returns 0/stale, the block read is zeros ⇒ `i_mode = 0`
> ⇒ `stat` reports type-0 = `?---------`, and descending into it gives ENOTDIR.

### 2d. Stale-checkpoint nid reuse compounds it across reboots
On reboot `next_free_nid` is restored from the stale checkpoint (CP+152), i.e.
rewound below the nids the crashed session already handed out. Nothing rescans
the NAT for the true maximum. The next session's `create_inode` therefore **reuses
the same nids**, so a surviving on-disk dentry from the crashed run and a brand-new
inode can collide on one nid — the "entries are unusable / cross-contaminated"
symptom. This is exactly why the notes correlate corruption with **crashed-session
image reuse**.

---

## 3. Every observation explained

| Observation | Explanation |
|---|---|
| **mkfs pre-created dirs always work** (/run/user/0 at mkfs:662; /root/.config etc. 5a18bc0) | They live in the base checkpoint written atomically by mkfs (§1). They never depend on runtime writeback, so no tear is possible. |
| **Fresh-image runtime mkdir *sometimes* works** (m6:114-116, single `create_dir_all(/root/.config)` succeeded) | With no crash, or when a checkpoint happens to fall after the op, coherent in-RAM state (§2a) makes it correct. The outcome is a race between "did a checkpoint capture it" and "did the VM die first." |
| **`?---------` after crashed-session image reuse** (m6:103-104, 955-957, 983-985) | Torn writeback across the hard kill (§2c) + nid reuse (§2d). The dominant, reproducible cause. |
| **BUG B: `mkdir -p /run/user/0` returns 0, deepest absent, 2nd call completes it** (m6:74-77) | Not an f2fs format bug — within-boot f2fs is coherent (§2a). Most likely (a) the tail-loss / torn-state variant observed after a state reset, or (b) a **kernel VFS dcache** negative/stale-lookup artifact between the two separate `mkdir()` syscalls, which is outside the f2fs server. Rated LOW confidence as a real f2fs bug; the repro in §5 disambiguates it. |
| **create_dir_all inside a `?` dir → ENOTDIR** | Parent resolves to a type-0 inode (`inode_is_dir` false, lib.rs:2190) ⇒ ENOTDIR (-20). Downstream of the same torn inode. |

---

## 4. Fix spec

**Root cause to fix (kernel-side, preferred): make directory-namespace
mutations durable atomically, so a hard kill can never expose a torn/`?-type`
entry.** mkfs is correct and needs no change.

### 4a. Minimal, targeted fix
Add an explicit `flush_checkpoint(ms)` at the successful end of the namespace
mutators, replacing reliance on the every-16 `maybe_flush`. At minimum
`handle_mkdir` (lib.rs:2207, before `ok_reply()`); for full coverage also
`handle_open` O_CREAT, `handle_symlink`, `handle_link`, `handle_rename`,
`handle_rmdir`, `handle_unlink`. Because the single-threaded server's in-RAM state
is always consistent between requests (§2a), a checkpoint taken at the end of an
op writes a **consistent** image every time — the dentry, child inode, NAT entry,
`next_free_nid`, curseg pointers and SIT all reach disk together. This eliminates
both the tear (§2c) and the nid-reuse (§2d).

Cost: one full `cache.flush_all` + CP write + device flush per namespace op.
mkdir/create are rare vs. bulk `write`, and the volume/cache are tiny, so this is
acceptable. (Equivalent cheap alternative: drop the `maybe_flush` threshold to 1;
slightly wasteful since mkdir triggers it 3×, so the explicit per-handler flush is
cleaner.) Keep the existing `maybe_flush(16)` for bulk data writes.

### 4b. One ordering hardening inside `flush_checkpoint`
`flush_checkpoint` issues all data/metadata `write_block`s, then the CP block,
then a **single** `virtio_blk::flush` at the very end (lib.rs:708-712). If the
machine dies *inside* this function the CP may be durable before the data. Insert
a `virtio_blk::flush(ms.dev)` **between** `cache.flush_all` and the CP write so the
CP (the commit record) can never precede the data it commits. Small window, but
free to close.

### 4c. Cosmetic correctness (independent, low priority)
Set new-directory `i_links = 2` and increment the parent's link count in
`handle_mkdir` (and decrement on `rmdir`), matching mkfs (mkfs:990). Not required
to fix `?-type`; fixes `st_nlink`/fsck.

### 4d. Belt-and-suspenders (non-kernel)
The shipped mkfs pre-creation of /run/user/0 and /root/.{config,cache,local}
remains a valid safety net and should stay until 4a lands and is verified.

---

## 5. On-target repro recipe (for the tree wave)

Uses a fresh image each time. The discriminating variable is **a hard kill with
no clean unmount** between the mkdir and the re-mount.

**Test A — prove within-boot mkdir is correct (isolates BUG B):**
Fresh image, boot, at the shell run as ONE line (no reboot, no kill):
```
mkdir -p /run/user/0 && ls -la /run/user && stat /run/user/0
```
Expect a proper `drwx` directory. If so, there is **no** within-boot f2fs mkdir
bug and BUG B was a crash/dcache artifact. If the deepest is absent/`?`, escalate
to a genuine within-boot bug (then look at the kernel VFS dcache, not f2fs).

**Test B — reproduce the `?---------` corruption (crash-consistency):**
1. Fresh image, boot to shell.
2. `mkdir /root/z1` (one op = 3 dirty writes, below the 16 threshold ⇒ **no
   checkpoint**).
3. Generate cache pressure so the parent (`/root`) dentry block is evicted while
   the new NAT/inode block is not, e.g. `ls -la / /bin /etc /usr >/dev/null`.
4. **Hard-kill QEMU from the host** (`kill -9` the qemu pid / tear down HVF).
   Do NOT `poweroff`/clean-shutdown — that would run `unmount()` and checkpoint.
5. Reboot the **same** image; `stat /root/z1` and `ls -la /root`.
   Expect `?---------` (type-0) or a dangling/ENOTDIR entry. A hard kill between
   the mkdir and the re-mount is **required** to reproduce.

**Test C — clean control (proves durability closes it):**
Same as B but after step 2 run `sync` (drives VFS_FSYNC → `flush_checkpoint`)
before the hard kill. On reboot `/root/z1` must be an intact `drwx` dir. With fix
4a applied, Test B alone (no explicit sync) must also yield an intact dir.

---

## Summary for orchestrator

- **Divergence:** none in the on-disk format — kernel and mkfs agree on dentry
  bitmap/slot/name layout, inode base (364), NAT formula, and next_free_nid. The
  real divergence is **temporal**: `handle_mkdir`/create/etc. are not
  synchronously checkpointed, and the 4-slot writeback cache tears an operation
  across a hard kill.
- **Each observation:** pre-created dirs work (in the base checkpoint); fresh
  mkdir sometimes works (coherent-until-checkpoint race); `?-type` after
  crash/reuse (torn writeback + stale-checkpoint nid reuse); ENOTDIR (descending
  a torn type-0 inode); BUG B (crash/dcache artifact, not an f2fs format bug).
- **Fix:** call `flush_checkpoint` at the end of the namespace mutators
  (min: `handle_mkdir` lib.rs:2207), add a flush between data and CP in
  `flush_checkpoint` (lib.rs:708), optionally fix dir nlink. mkfs needs no change.
- **Crash-consistency answer:** yes — a checkpoint (fsync-equivalent) at mkdir is
  needed; that is the fix.
- **Confidence:** crash-consistency root cause HIGH; separate within-boot mkdir
  bug LOW (Test A settles it).
