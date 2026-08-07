# Gap 2 — kernel instrumentation plan (repaint-in-place memfd shows stale pixels)

STATUS: ready to apply. Written 2026-08-02 from a read-only analysis pass; **no
file in `/Users/forain/code/leandros` was modified** while producing it.

Prerequisite reading: `memfd-shm-findings.md` (same directory). FACTS 1–4 there
are settled and are *not* re-tested by this plan. This plan measures only what is
left:

- **(a)** smithay/cosmic-comp caches the imported SHM texture across frames
  despite a fresh `wl_buffer` + full `damage_buffer`.
- **(b)** the compositor's pool mapping is established by a route different from
  scmtest's — specifically, the fd arriving over the Wayland socket's SCM_RIGHTS
  import does not keep `VnodeKind::TmpFile { idx }` in the **compositor's** fd
  table, so its `mmap` misses the K1 aliasing branch and gets a private copy.
- **(c)** `wl_shm_pool.create_buffer` offset/extent handling.

The whole design rests on one observation that makes the decisive measurement
cheap and safe: **the kernel can read the pool's contents directly from the VMO's
physical frames through the HHDM.** That is kernel memory — always mapped, never
demand-paged — so a periodic content checksum can be taken from IRQ context
without ever touching a user address. If that checksum advances once a second
while the screen is frozen, the entire kernel/memory half of Gap 2 is exonerated
in one line of serial output.

---

## 0. Shared plumbing — `mm::gap2`

Three crates need the same switch and the same writer: `kernel`, `servers/vfs`
and `mm::vmm`. `mm` is the only crate all three depend on
(`kernel/Cargo.toml`, `servers/vfs/Cargo.toml` both list `mm`), so the module
goes there. It needs **no** `Cargo.toml` change: `arch_serial_putc` is a
`#[no_mangle]` symbol in `kernel/src/main.rs:144`, resolved at link time, and
`sched/src/lib.rs:1263` / `signal.rs:776` already declare it this way from a
non-`drivers` crate.

Precedent for the style: `drivers/src/drm_device_interface.rs:606-617`
(`DRM_STATS`, commit `b3659fa`) — a `pub const bool` compile-time gate whose
output goes **straight to the UART, bypassing `CONSOLE_OUT_LOCK`**.

> **Do not use `crate::serial_print_str`** anywhere in this plan.
> `serial_print_str` (kernel/src/main.rs:244) goes through `with_console_lock`
> **and** `serial_write_byte` (:109), which mirrors every byte into
> `drivers::framebuffer::fb_putc` + `fb_flush`. During a COSMIC session that
> paints kernel text over the live desktop and re-flushes the scanout — it would
> corrupt the very thing being measured. `arch_serial_putc` →
> `serial_write_byte_direct` (:136) is UART-only and lock-free.

### 0.1 NEW FILE `mm/src/gap2.rs`

```rust
//! Gap-2 instrumentation: why does a memfd + MAP_SHARED wl_shm pool that is
//! repainted in place show the compositor stale pixels forever?
//!
//! Compile-time gated, UART-direct serial tracing shared by `kernel`,
//! `servers/vfs` and `mm::vmm` — `mm` is the only crate all three depend on.
//! Modeled on `drivers::drm_device_interface::DRM_STATS`: output goes straight
//! to the UART via `arch_serial_putc`, bypassing CONSOLE_OUT_LOCK and the
//! framebuffer console, so it is callable from IRQ context and from under leaf
//! spinlocks without deadlocking or painting over the live desktop.
//!
//! Everything here prints integers and kernel-side byte slices ONLY. No call in
//! this module may ever be handed a user pointer.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Master switch. Flip to `true` to trace, rebuild release, run the session.
pub const ON: bool = false;

/// Also trace `AddressSpace::map_shared_frames`. That site runs with the
/// address space `busy` flag held (see the plan's safety section); the
/// information is redundant with `[G2MAP]`. Leave `false` unless `[G2MAP]`
/// comes back inconclusive.
pub const MAP_SHARED_FRAMES_TRACE: bool = false;

/// Hard ceiling on event lines, so a mis-aimed filter cannot flood a session.
/// The 0.5 Hz `[G2SUM]` sampler does NOT draw from this budget.
static BUDGET: AtomicUsize = AtomicUsize::new(400);

/// tmpfs slot index of the pool being watched by the `[G2SUM]` sampler.
/// `usize::MAX` = not yet identified.
static WATCH_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);

/// True at most `BUDGET` times, and only when `ON`. Every event site calls this
/// as its gate so the budget is shared across all of them.
pub fn budget_ok() -> bool {
    if !ON { return false; }
    BUDGET
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed,
                      |v| if v == 0 { None } else { Some(v - 1) })
        .is_ok()
}

pub fn watch_idx() -> usize { WATCH_IDX.load(Ordering::Relaxed) }

/// First writer wins: the applet's pool is the first `leandros-applet` memfd.
pub fn set_watch(idx: usize) {
    let _ = WATCH_IDX.compare_exchange(usize::MAX, idx,
                                       Ordering::Relaxed, Ordering::Relaxed);
}

extern "C" { fn arch_serial_putc(c: u8); }

pub fn s(m: &str) { for &b in m.as_bytes() { unsafe { arch_serial_putc(b) } } }

/// Print a KERNEL-side byte slice (a path buffer, a memfd name). Never a user
/// pointer — the caller must have copied it into kernel memory already.
pub fn bytes(b: &[u8]) {
    for &c in b {
        let c = if (0x20..0x7f).contains(&c) { c } else { b'.' };
        unsafe { arch_serial_putc(c) }
    }
}

/// Compact hex, no leading zeros (keeps lines short on a polled UART).
pub fn h(v: usize) {
    s("0x");
    let mut started = false;
    for i in (0..16).rev() {
        let d = ((v >> (i * 4)) & 0xF) as u8;
        if d != 0 { started = true; }
        if started || i == 0 {
            unsafe { arch_serial_putc(if d < 10 { b'0' + d } else { b'a' + d - 10 }) }
        }
    }
}

pub fn kv(k: &str, v: usize) { s(k); h(v); }
pub fn nl() { s("\n"); }
```

### 0.2 `mm/src/lib.rs` — one line

Existing module block is lines 11–16. Insert alphabetically:

```rust
 pub mod cow;
+pub mod gap2;
 pub mod pageref;
```

---

## 1. Log points

Sites are listed in the order the events fire during a session. Every site's
gate is `mm::gap2::budget_ok()` (which is itself `ON`-gated), except the sampler.

### Site M — `[G2MEMFD]`: identify the applet, its pid, and its tmpfs slot

**File** `kernel/src/syscall.rs` · **fn** `sys_memfd_create` (declared :6847) ·
**insert after** the existing `vfs::mark_memfd(pid, fd as usize);` at **:6903**,
inside the `if fd >= 0 {` block.

Everything needed is already in scope: `pid` (:6864), `fd` (:6895), and `path` /
`plen`, the **kernel-side** copy of the name built at :6867-6885 (the user
pointer `name_ptr` was already dereferenced into `path` at :6873 — do not
re-read it).

```rust
            vfs::mark_memfd(pid, fd as usize);
+           // [GAP2] Name the memfd, its owner pid and its tmpfs slot, so every
+           // later [G2*] line can be attributed to a process and a pool. Also
+           // arms the [G2SUM] content sampler on the applet's pool.
+           if mm::gap2::ON {
+               let g2_idx = vfs::gap2_tmpfile_idx(pid, fd as usize)
+                   .unwrap_or(usize::MAX);
+               mm::gap2::s("[G2MEMFD] pid="); mm::gap2::h(pid as usize);
+               mm::gap2::kv(" fd=",  fd as usize);
+               mm::gap2::kv(" idx=", g2_idx);
+               mm::gap2::s(" name="); mm::gap2::bytes(&path[..plen]);
+               mm::gap2::nl();
+               // The applet's pool is the one to sample. Its memfd name is
+               // "leandros-applet" (m7w-applet/src/main.rs:138), so the tmpfs
+               // path is "/tmp/memfd:leandros-applet".
+               if g2_idx != usize::MAX
+                   && path[..plen].starts_with(b"/tmp/memfd:leandros-applet")
+               {
+                   mm::gap2::set_watch(g2_idx);
+               }
+           }
```

`let _ = plen;` at :6886 exists only to silence an unused warning and may be
left in place.

**Volume**: one line per `memfd_create` in the whole session (tens).
**Locks held**: none (`vfs::handle` has already returned). SAFE.

### Site E — `[G2IMP]`: does the compositor's imported fd keep `TmpFile { idx }`?

This is the direct test of hypothesis **(b)**.

**File** `servers/vfs/src/lib.rs` · **fn** `import_fd` (**:3418**).

Current body assigns `tbl.fds[slot]` at :3428 and returns at :3429. `tbl` is a
`&mut` borrow of the `tbls` guard, so the guard must be dropped before logging
(see §4 — do not hold `FD_TABLES` across a polled-UART write).

```rust
     tbl.fds[slot] = FdEntry { kind: tf.kind, flags, in_use: true };
+    drop(tbls);
+    // [GAP2] SCM_RIGHTS hand-off of a tmpfs/memfd fd — this is exactly the
+    // Wayland wl_shm pool arriving in the compositor. If `idx` here does not
+    // match the sender's, or the kind is not TmpFile, the receiver's mmap will
+    // miss the K1 aliasing branch (hypothesis (b)).
+    if let VnodeKind::TmpFile { idx, writable, .. } = tf.kind {
+        if mm::gap2::budget_ok() {
+            mm::gap2::s("[G2IMP] rxpid="); mm::gap2::h(pid as usize);
+            mm::gap2::kv(" fd=",    slot);
+            mm::gap2::kv(" idx=",   idx);
+            mm::gap2::kv(" w=",     writable as usize);
+            mm::gap2::kv(" flags=", flags as usize);
+            mm::gap2::nl();
+        }
+    }
     slot as isize
 }
```

**Filter**: `TmpFile` only — pipes, sockets and eventfds crossing SCM_RIGHTS are
ignored. **Volume**: a handful per session.
**Locks held at the log**: none after `drop(tbls)`. Caller (`servers/net/src/lib.rs:1850`)
has already dropped the `conns` lock (:1830, with a comment saying so). SAFE.

### Site C — `[G2ACQ]`: what the VMO looked like when frames were handed out

**File** `servers/vfs/src/lib.rs` · **fn** `vmo_acquire_frames` (**:537**).

Two edits.

(1) Capture whether this call *promoted* the inode. Immediately before the
existing `if vmos[idx].is_none() {` at **:550**:

```rust
+    let g2_promoted = vmos[idx].is_none();
     if vmos[idx].is_none() {
```

(2) Replace the tail (**:580-588**). The values are read while `vmo` is still
borrowed, then **both guards are dropped before printing**:

```rust
     let mut out = alloc::vec::Vec::with_capacity(n);
     for p in first..need_pages {
         let phys = vmo.pages[p];
         mm::pageref::inc(phys);
         out.push(phys);
     }
+    let g2_np    = vmo.pages.len();
+    let g2_vlen  = vmo.len;
+    let g2_memfd = vmo.is_memfd;
+    let g2_borrow = vmo.borrowed;
+    let g2_p0    = out.first().copied().unwrap_or(0);
+    // Release TMP_VMOS/TMP_FILES *before* the serial write: a ~100-char line on
+    // a polled UART is milliseconds of busy-wait, and these two mutexes gate
+    // every tmpfs operation in the system.
+    drop(vmos);
+    drop(tmp);
+    if mm::gap2::budget_ok() {
+        mm::gap2::s("[G2ACQ] pid="); mm::gap2::h(pid as usize);
+        mm::gap2::kv(" fd=",    fd);
+        mm::gap2::kv(" idx=",   idx);
+        mm::gap2::kv(" off=",   off);
+        mm::gap2::kv(" len=",   len);
+        mm::gap2::kv(" prom=",  g2_promoted as usize);
+        mm::gap2::kv(" memfd=", g2_memfd as usize);
+        mm::gap2::kv(" borrow=", g2_borrow as usize);
+        mm::gap2::kv(" np=",    g2_np);
+        mm::gap2::kv(" vlen=",  g2_vlen);
+        mm::gap2::kv(" p0=",    g2_p0);
+        mm::gap2::nl();
+    }
     Some(out)
 }
```

NLL ends the `vmo` borrow after its last read, so `drop(vmos)` compiles.

**Why `prom=` matters**: `prom=1` on the *second* mapper of a pool would mean the
VMO was rebuilt after the first mapper took its frames — the "two frame lists for
one inode" failure (decision-table row 4/6c).

### Site A — `[G2MAP]`: the K1 shared-alias branch (the good path)

**File** `kernel/src/syscall.rs` · **fn** `sys_mmap` · **lines 1659-1679**.

Two changes: bind `idx` out of the pattern (currently `TmpFile { .. }`), and log
**after** `with_current_address_space_mut` returns.

```rust
     if flags & MAP_SHARED != 0 {
-        if let Some(vfs::VnodeKind::TmpFile { .. }) = kind {
+        if let Some(vfs::VnodeKind::TmpFile { idx, .. }) = kind {
             match vfs::vmo_acquire_frames(pid, fd, off, len) {
                 Some(frames) => {
+                    // [GAP2] snapshot before the frames are consumed by the map.
+                    let g2_p0 = frames.first().copied().unwrap_or(0);
+                    let g2_n  = frames.len();
                     let mapped = with_current_address_space_mut(|as_| {
                         if flags & MAP_FIXED != 0 { as_.unmap_range(virt, len); }
                         as_.map_shared_frames(virt, &frames, page_flags)
                     });
+                    // Logged OUTSIDE the closure: inside it the address space
+                    // `busy` flag is held (see the plan's safety section).
+                    if mm::gap2::budget_ok() {
+                        mm::gap2::s("[G2MAP] pid="); mm::gap2::h(pid as usize);
+                        mm::gap2::kv(" fd=",   fd);
+                        mm::gap2::kv(" idx=",  idx);
+                        mm::gap2::kv(" off=",  off);
+                        mm::gap2::kv(" len=",  len);
+                        mm::gap2::kv(" prot=", prot);
+                        mm::gap2::kv(" mflg=", flags);
+                        mm::gap2::kv(" virt=", virt);
+                        mm::gap2::kv(" p0=",   g2_p0);
+                        mm::gap2::kv(" n=",    g2_n);
+                        mm::gap2::kv(" rc=",   match mapped {
+                            Some(true) => 1, Some(false) => 0, None => 2 });
+                        mm::gap2::nl();
+                    }
                     return match mapped {
```

`prot`, `flags`, `virt`, `off`, `len`, `fd`, `pid` are all already in scope in
`sys_mmap` (`prot` is used at :1757, `virt` computed above :1562).

**Volume**: one line per `MAP_SHARED` mmap of a tmpfs/memfd fd — i.e. per
wl_shm pool mapping, on either side. Dozens per session.

### Site B — `[G2FALL]`: MAP_SHARED that fell through to the eager private copy

This is the other half of hypothesis **(b)**: if the compositor's pool fd appears
here instead of in `[G2MAP]`, its mapping is a **snapshot copied at mmap time**
and can never see later stores.

**File** `kernel/src/syscall.rs` · **fn** `sys_mmap` · **insert at :1681**,
immediately before the `// Normal file-backed mmap follows...` comment.

```rust
+    // [GAP2] A MAP_SHARED file mapping that did NOT take the K1 aliasing branch
+    // above is about to get an EAGER PRIVATE COPY (the read loop below). If the
+    // Wayland shm-pool fd shows up here, that mapping is frozen at frame 0.
+    if flags & MAP_SHARED != 0 && mm::gap2::budget_ok() {
+        mm::gap2::s("[G2FALL] pid="); mm::gap2::h(pid as usize);
+        mm::gap2::kv(" fd=",   fd);
+        mm::gap2::kv(" kind=", vfs::gap2_kind_tag(&kind));
+        mm::gap2::kv(" off=",  off);
+        mm::gap2::kv(" len=",  len);
+        mm::gap2::kv(" prot=", prot);
+        mm::gap2::nl();
+    }
     // Normal file-backed mmap follows...
```

`kind` is the `Option<VnodeKind>` from :1599 and is still live.

**Volume risk**: this site is *not* restricted to tmpfs, because the whole point
is to catch an fd whose kind is wrong. `MAP_SHARED` file mappings are rare
outside shm (the loader uses `MAP_PRIVATE`), and the shared 400-line budget caps
it regardless. If it does turn out noisy, tighten to
`&& !matches!(kind, Some(vfs::VnodeKind::MountedFile { .. }))`.

### Site D — `[G2MSF]`: inside `map_shared_frames` (OPTIONAL, default off)

**File** `mm/src/vmm.rs` · **fn** `AddressSpace::map_shared_frames` (**:399**) ·
**insert at :463**, just before the closing `true`.

```rust
             cow:       false,
         });
+        // [GAP2] OPTIONAL. This runs with the address space `busy` flag held —
+        // it must print integers only and must never dereference `virt`.
+        // `mm` does not depend on `sched`, so there is no pid here; correlate by
+        // `p0` against the [G2MAP] line the caller emits a moment later.
+        if crate::gap2::ON && crate::gap2::MAP_SHARED_FRAMES_TRACE {
+            crate::gap2::s("[G2MSF] virt="); crate::gap2::h(virt);
+            crate::gap2::kv(" p0=", frames[0]);
+            crate::gap2::kv(" n=",  pages);
+            crate::gap2::kv(" w=",  flags.contains(PageFlags::WRITABLE) as usize);
+            crate::gap2::nl();
+        }
         true
```

Leave `MAP_SHARED_FRAMES_TRACE = false` on the first run. Everything it reports
is already in `[G2MAP]`; turn it on only if `[G2MAP]`'s `rc=` disagrees with what
the mapping actually looks like.

### Site F — `[G2SUM]`: the decisive one — does the pool's memory actually change?

A 0.5 Hz checksum of the watched pool's **physical frames**, read through the
HHDM. It answers "are the applet's per-second stores reaching the shared VMO at
all?" without any user-memory access and without any cooperation from either
userspace process.

#### F.1 New VFS probe — `servers/vfs/src/lib.rs`, next to `vmo_release_frames` (:594)

```rust
/// [GAP2] Read-only content probe of a tmpfs/memfd VMO.
/// Returns `(npages, first_phys, checksum, vmo_len)`.
///
/// SAFE FROM IRQ CONTEXT, and only because of two properties:
///   * `try_lock` ONLY. `TMP_VMOS` is taken from syscall context with interrupts
///     enabled, so a blocking `lock()` from the timer tick would deadlock
///     against a holder preempted on this same CPU.
///   * It reads the frames through `mm::phys_to_virt` — the HHDM identity map,
///     i.e. kernel memory that is always present. It never touches a user
///     virtual address, so it cannot demand-page and cannot re-enter the fault
///     handler (project rule: never touch user memory from IRQ/lock context).
pub fn vmo_debug_probe(idx: usize) -> Option<(usize, usize, u64, usize)> {
    if idx >= MAX_TMP_FILES { return None; }
    let vmos = TMP_VMOS.try_lock()?;
    let vmo = vmos[idx].as_ref()?;
    let np = vmo.pages.len();
    if np == 0 { return Some((0, 0, 0, vmo.len)); }
    // Checksum the WHOLE buffer. Do not "optimise" this to the first page: the
    // applet's clock glyphs are vertically centred (rows 5..26 of 32 at an
    // 880-byte stride, m7w-applet/src/main.rs), so page 0 is uniform background
    // and a page-0-only checksum would be constant even while the clock ticks.
    let total = vmo.len.min(np * 4096);
    let mut sum: u64 = 0;
    let mut off = 0usize;
    while off + 4 <= total {
        let phys = vmo.pages[off / 4096];
        if phys != 0 {
            let p = (mm::phys_to_virt(phys) + (off % 4096)) as *const u32;
            sum = sum.wrapping_mul(31)
                     .wrapping_add(unsafe { core::ptr::read_volatile(p) } as u64);
        }
        off += 4;
    }
    Some((np, vmo.pages[0], sum, vmo.len))
}
```

For the applet's 220×32 XRGB pool that is 28160 bytes = `0x6e00` → 7040 volatile
reads, tens of microseconds, twice a second.

#### F.2 Two small VFS accessors (same file, next to `tmpfile_owner_of` :459)

```rust
/// [GAP2] Public wrapper over `tmpfile_owner_of` for the instrumentation.
pub fn gap2_tmpfile_idx(pid: u32, fd: usize) -> Option<usize> {
    tmpfile_owner_of(pid, fd)
}

/// [GAP2] Stable small tag per VnodeKind, for one-line traces.
pub fn gap2_kind_tag(k: &Option<VnodeKind>) -> usize {
    match k {
        None => 0,
        Some(VnodeKind::None) => 1,
        Some(VnodeKind::RamFile { .. }) => 2,
        Some(VnodeKind::TmpFile { .. }) => 3,
        Some(VnodeKind::MountedFile { .. }) => 4,
        Some(VnodeKind::Pipe { .. }) => 5,
        Some(VnodeKind::DynamicDevice { .. }) => 6,
        Some(_) => 7,
    }
}
```

(`3` appearing in a `[G2FALL]` line would be self-contradictory and is itself a
finding — see decision table row 9.)

#### F.3 Tick hook — `kernel/src/syscall.rs`

`MAX_TICK_HOOKS` is **4** (`sched/src/lib.rs:1165`) and **three are already
taken**: `servers/drm/src/lib.rs:146` (`drm_tick`),
`servers/pipewire/src/lib.rs:177` (`tick_pump`), `kernel/src/init.rs:84`
(`poll_deadline_tick`). Do **not** consume the last slot — piggyback on
`poll_deadline_tick` (**:6625**) instead:

```rust
 pub fn poll_deadline_tick() {
     use core::sync::atomic::Ordering::Relaxed;
+    gap2_sample_tick();
     let now = ticks();
```

and add, immediately above it:

```rust
/// [GAP2] 0.5 Hz content probe of the watched wl_shm pool.
///
/// IRQ CONTEXT (BSP timer tick, via `sched::timer_tick_irq`). It may only:
/// try_lock, read HHDM (kernel) memory, and write the UART directly. It must
/// NOT touch user memory, must NOT take RUN_QUEUE, and must NOT wake anything.
fn gap2_sample_tick() {
    use core::sync::atomic::Ordering::Relaxed;
    if !mm::gap2::ON { return; }
    let idx = mm::gap2::watch_idx();
    if idx == usize::MAX { return; }
    static LAST: AtomicUsize = AtomicUsize::new(0);
    let now = ticks() as usize;                  // 100 Hz
    if now.wrapping_sub(LAST.load(Relaxed)) < 50 { return; }   // 0.5 Hz
    LAST.store(now, Relaxed);
    if let Some((np, p0, sum, vlen)) = vfs::vmo_debug_probe(idx) {
        mm::gap2::s("[G2SUM] t="); mm::gap2::h(now);
        mm::gap2::kv(" idx=",  idx);
        mm::gap2::kv(" np=",   np);
        mm::gap2::kv(" p0=",   p0);
        mm::gap2::kv(" vlen=", vlen);
        mm::gap2::kv(" sum=",  sum as usize);
        mm::gap2::nl();
    }
}
```

`AtomicUsize` and `ticks()` are already imported in `kernel/src/syscall.rs`
(`AtomicUsize::new(1)` at :6859; `ticks()` at :6627).

### Site G — `[G2UNMAP]` (OPTIONAL, only if row 6 of the decision table hits)

If `[G2SUM]` turns out to be constant while `[G2MAP] pid=<applet>` looks correct,
the next question is whether the applet's mapping was silently replaced. Add to
`sys_unmap_mem` (**kernel/src/syscall.rs:1764**), before the `with_current_address_space_mut`
call:

```rust
+    if mm::gap2::ON && mm::gap2::watch_idx() != usize::MAX && mm::gap2::budget_ok() {
+        mm::gap2::s("[G2UNMAP] pid="); mm::gap2::h(current_pid() as usize);
+        mm::gap2::kv(" virt=", virt);
+        mm::gap2::kv(" len=",  size);
+        mm::gap2::nl();
+    }
```

Leave this out of the first run.

---

## 2. What to print, and where each value comes from

| Field | Meaning | Source at the site |
|---|---|---|
| `pid` / `rxpid` | owning (or receiving) process | Site A/B: `let pid = current_pid();` already at syscall.rs:1596. Site C/E: the `pid: u32` parameter. Site M: `pid` at :6864. |
| `fd` | descriptor in **that pid's** table | Site A/B/C: the `fd` parameter. Site E: the freshly allocated `slot`. Site M: the `fd` returned by `VFS_OPEN`. |
| `idx` | tmpfs slot = **inode identity** | Site A: bind it out of the pattern (`TmpFile { idx, .. }`). Site C: the `idx` local from `tmpfile_owner_of` (:541). Site E: destructure `tf.kind`. Site M: `vfs::gap2_tmpfile_idx(pid, fd)`. **This is the join key between the two processes.** |
| `virt` | mapping virtual base | Site A: the `virt` local (computed at :1556-1562). Site D: the page-aligned `virt` (vmm.rs:418). |
| `len` | mapping length | the `len` parameter at each site. |
| `off` | file offset of the mapping | the `off` parameter. Site C additionally derives `first = off/4096` (:542). |
| `prot` / `mflg` | PROT_* / MAP_* the caller asked for | `prot` and `flags` parameters of `sys_mmap`. |
| `p0` | **physical address of the first mapped frame** | Site A: `frames.first().copied()` — `vmo_acquire_frames` returns the frame list in order (vfs :581-586). Site C: `out.first()`. Site D: `frames[0]`. Site F: `vmo.pages[0]`. **Comparing `p0` across two pids for the same `idx` is the identity test.** |
| `n` / `np` | frame count | `frames.len()` (Site A/D); `vmo.pages.len()` (Site C/F). |
| `prom` | did this call promote a plain tmpfs inode into a VMO | the `g2_promoted` capture before vfs:550. |
| `memfd` / `borrow` | `TmpVmo.is_memfd` / `.borrowed` | fields of the `vmo` reference (vfs:568). `borrow=1` would mean a DRM dmabuf VMO, not an shm pool. |
| `vlen` | VMO EOF | `vmo.len`. Cross-check against the applet's `0x6e00`. |
| `sum` | rolling `sum*31 + word` over the whole buffer | `vmo_debug_probe`, HHDM reads. |
| `rc` | 1 = mapped, 0 = ENOMEM, 2 = no address space | `match mapped` at Site A. |
| `kind` | VnodeKind tag of a fallback fd | `vfs::gap2_kind_tag(&kind)`. |
| `name` | memfd path | `path[..plen]`, the kernel-side buffer built at syscall.rs:6867-6885. |

Correlation recipe from the serial log:

1. `[G2MEMFD] … name=/tmp/memfd:leandros-applet` → gives **A** (applet pid) and
   **I** (tmpfs idx).
2. `[G2MAP] pid=A idx=I` → applet's `p0_A`, `off`, `len`, `virt`.
3. `[G2IMP] rxpid=C idx=I` → **C** (cosmic-comp pid). If `idx != I` here, stop:
   hypothesis (b) is proven at the hand-off itself.
4. `[G2MAP] pid=C idx=I` → compositor's `p0_C`, `off`, `len`.
5. `[G2SUM]` series over the run.

There is deliberately **no** process-name lookup: `sched::Task` (sched/src/task.rs:108)
carries no `comm` field, so identity comes from the memfd name plus the SCM_RIGHTS
receiver.

---

## 3. DECISION TABLE

Read the serial log, answer three questions — (Q1) does `sum` change across
`[G2SUM]` samples? (Q2) is there a `[G2MAP]` for the compositor pid `C` on idx
`I`? (Q3) does `p0_C == p0_A`? — then take the row.

| # | Observation | Verdict | Next step |
|---|---|---|---|
| 1 | `sum` **changes** each sample · `[G2MAP] pid=C idx=I` present · `p0_C == p0_A` · `off=0` · `len >= 0x6e00` on both sides | **(a) CONFIRMED by elimination.** The applet's per-second stores reach the shared frames, and the compositor maps *exactly those frames*. Every kernel link is proven live in situ. (b) KILLED. (c) killed at the kernel level. The staleness is entirely above `mmap`: smithay/cosmic-comp reads the pool once and reuses a cached texture. | Stop kernel work. Go to smithay's SHM import + renderer texture cache keying (`~/.cargo/git/checkouts/smithay-*/src/backend/renderer/`, `wayland/shm`), and run Phase 2 (§3.1) to identify the cache key. |
| 2 | `sum` **changes** · **no** `[G2MAP] pid=C` and **no** `[G2FALL] pid=C` for idx `I`, but `[G2IMP] rxpid=C idx=I` **is** present | The compositor received the fd and **never mapped it**. Not a coherence bug — a "the compositor never reads the memory" bug. Still (a)-family but a different sub-case from row 1. | Check whether smithay's `ShmPool` mmap is happening at all (it may be failing and being swallowed); then `WAYLAND_DEBUG=1` on cosmic-comp for `wl_shm.create_pool`. |
| 3 | `sum` **changes** · `[G2FALL] pid=C fd=… kind=<tag>` appears for the pool fd | **(b) CONFIRMED.** The compositor's imported fd did not resolve to `TmpFile`, so its `MAP_SHARED` fell through to the eager private copy at syscall.rs:1681-1760 — a snapshot of frame 0, frozen by construction. The `kind=` tag names what it became (`0` = fd unknown to the VFS in that pid's table). | Fix the fd-kind hand-off. Compare the `[G2IMP]` line's `idx` against `I`; if `[G2IMP]` never fired for `C`, the fd reached the compositor by some path other than `vfs::import_fd`. |
| 4 | `sum` **changes** · `[G2MAP] pid=C idx=I` present but `p0_C != p0_A` | Two different frame lists behind **one** inode. Kernel bug in `vmo_acquire_frames`. Check the `[G2ACQ]` lines for idx `I`: a `prom=1` on the second acquire, or `np` shrinking between acquires, is the smoking gun. | Fix `vmo_acquire_frames` frame-list stability across ftruncate/regrow; add a scmtest case for map-then-grow-then-map. |
| 5 | `sum` **changes** · `p0_C == p0_A` but `off != 0`, or `len_C < 0x6e00`, or `vlen != 0x6e00` | **(c) region.** The compositor's window into the pool does not cover the buffer the applet is drawing (the applet uses `create_buffer(offset=0, 220, 32, stride=880)`, i.e. exactly `0x6e00` bytes). | Compare `len_C` with the `wl_shm.create_pool` size; look at smithay's pool-resize path. |
| 6 | `sum` is **constant** across ≥3 samples, and `[G2MAP] pid=A idx=I` is present with `p0_A ==` the sampler's `p0` | The applet's mapping is correct but **nothing is being written through it**. Contradicts SETTLED FACT 3 *in the real session*. The bug is client-side, not compositor-side. | Move to userspace: unconditional `eprintln!` of `clock_text()` per tick in the applet, confirm `draw()` re-runs and `time()` advances **inside the panel's environment** (it may differ from the standalone test). |
| 6a | `sum` constant · `[G2FALL] pid=A` present for the pool fd | The **applet's own** mmap took the private-copy path — its writes go to a private page nobody else can see. K1 gate bug on the *creator* side. | Inspect why `vfs_get_node_kind(A, fd)` did not return `TmpFile` at syscall.rs:1599. |
| 6b | `sum` constant · sampler `p0` differs from `p0_A` | The VMO's frame list was rebuilt **after** the applet mapped; the applet writes orphaned frames. | Look for a later `[G2ACQ] prom=1` or a `ftruncate` on idx `I` after the applet's `[G2MAP]`. Enable Site G. |
| 7 | `sum` changes **exactly once** then freezes | The applet drew frame 0 and then stopped repainting (a stuck event loop or a blocking `wl_display` flush), not a memory problem. | Userspace: the applet's poll loop and `wl_display` round-trip; check for `Broken pipe`/`EAGAIN` in the session log. |
| 8 | No `[G2MEMFD]` line whose `name=` contains `leandros-applet` | The applet never reached `memfd_create`, or the staged binary is not the clock build. | Confirm staging and the `Starting: /bin/leandros-applet` / `committed 220x32 clock` lines before touching anything else. |
| 8a | `[G2MEMFD]` present with a good `idx`, but `[G2SUM]` never appears | The watch was never armed (name prefix mismatch — check the printed `name=`), or `TMP_VMOS.try_lock()` is permanently contended. | Read the actual `name=` from the log and adjust the `starts_with` prefix; if that is fine, temporarily call `mm::gap2::set_watch(<idx from the log>)` at Site A instead. |
| 9 | `[G2FALL]` with `kind=0x3` (TmpFile) | Self-contradictory: the K1 branch at :1659 tests the same `kind` value. Means `flags & MAP_SHARED` was re-read differently, or the `[G2FALL]` insertion landed on a path the K1 branch can also fall out of. | Re-check the edit placement — Site B must be *after* the closing brace of the `if flags & MAP_SHARED != 0 { if let … }` block at :1679. |

### 3.1 Phase 2 — only if row 1 hits (identifies the compositor's cache key)

Row 1 leaves "the compositor caches" as the conclusion but not *what it keys on*.
One userspace A/B settles it, and it is a change to
`/Users/forain/code/leandros-artifacts/m7w-applet/src/main.rs` — **outside** the
kernel tree:

- Allocate the pool at `2 * WIDTH * HEIGHT * 4` and `mmap` all of it.
- Each frame, draw into half `f & 1` and call
  `create_buffer(offset = (f & 1) * WIDTH * HEIGHT * 4, …)`.

| Result | Meaning |
|---|---|
| Clock ticks | The compositor's texture cache is keyed on `(pool, offset)` (or on "same backing memory ⇒ same texture"). (a) confirmed with the key identified; a two-slot pool is the immediate workaround and the smithay cache is the real fix. Also proves `create_buffer` offset handling is correct, closing (c). |
| Still frozen | The cache is keyed on the pool/fd alone, or the compositor never re-reads shm at all after the first import. Escalate into smithay's `import_shm_buffer` / `MemoryRenderBuffer` damage handling. |

Free companion signal (no code change): with `WAYLAND_DEBUG=1` on the applet,
check whether `wl_buffer.release` arrives for each frame. The applet destroys the
old buffer without waiting for release (main.rs:196-198), so a compositor that
never releases is holding a reference — consistent with, and corroborating, row 1.

---

## 4. Logging discipline

The session runs dozens of processes and the UART is polled per byte, so volume
control is a correctness requirement, not neatness.

1. **One compile-time switch.** `mm::gap2::ON`. With it `false` every site
   compiles to nothing (`budget_ok()` returns `false` on a `const` test; LLVM
   drops the block). This mirrors `DRM_STATS`
   (`drivers/src/drm_device_interface.rs:617`, commit `b3659fa`) — same
   discipline, same UART-direct output path.
2. **UART-direct, no console lock, no framebuffer.** Everything goes through
   `arch_serial_putc` (kernel/src/main.rs:144 → `serial_write_byte_direct` :136).
   Never `serial_print_str` — it takes `CONSOLE_OUT_LOCK` **and** mirrors to
   `fb_putc`/`fb_flush`, which would paint over the live desktop and perturb the
   measurement.
3. **Filter to the objects that matter.** Sites A, C, E fire only for
   tmpfs/memfd (`VnodeKind::TmpFile`) — that is `memfd`s and wl_shm pools, which
   are created a few dozen times per session, not per frame. Site B is
   `MAP_SHARED`-only. Site M is `memfd_create`-only.
4. **Global 400-line budget** shared by all event sites
   (`mm::gap2::budget_ok()`), so a mis-aimed filter degrades to a truncated log
   rather than a hung session.
5. **The sampler is rate-limited, not budgeted**: one line per 50 ticks = 0.5 Hz.
   A 150 s run yields ~75 `[G2SUM]` lines — enough to see a per-second clock
   change, small enough to read.
6. **Expected total** for a 150 s session: well under 200 lines.
7. **Distinct greppable prefixes**: `[G2MEMFD] [G2IMP] [G2ACQ] [G2MAP] [G2FALL]
   [G2SUM] [G2MSF] [G2UNMAP]`. Capture with the existing persistent
   serial-socket reader (`m7w_run.py`) and post-filter with `grep '^\[G2'` —
   the serial port drops output when nothing is attached
   (`feedback_qemu_serial_capture`).
8. **Hex everywhere, no leading zeros.** Decimal formatting costs more code and
   more UART bytes; `0x6e00` is the number to recognise (28160 = the applet's
   pool size).
9. **Revert plan**: the whole change is one new file plus additive blocks under
   `if mm::gap2::…` guards, except three genuinely structural edits — the
   `TmpFile { .. }` → `TmpFile { idx, .. }` pattern at syscall.rs:1660, the
   `drop(vmos)/drop(tmp)` restructure in `vmo_acquire_frames`, and the
   `drop(tbls)` in `import_fd`. Those three are behaviour-preserving and can be
   kept; the rest can be deleted wholesale.

---

## 5. Safety review — locks and the "never touch user memory" rule

Project rule (`project_user_mem_under_spinlock`, commit `82d0cc3`): never touch
user memory under RUN_QUEUE / IRQ-off spinlocks — a demand-paging fault re-enters
the scheduler lock and freezes every vCPU with no panic.

**No site in this plan reads or writes user memory.** Every value printed is
either a kernel local, a field of a kernel structure, or a physical frame read
through the HHDM. That is the property that makes even the IRQ-context sampler
safe. Site M prints `path[..plen]`, which is the **kernel-side** copy the syscall
already made at syscall.rs:6873 — do not "simplify" it to print from `name_ptr`.

| Site | Context | Locks held at the log statement | Verdict |
|---|---|---|---|
| M — `[G2MEMFD]` | syscall | none (`vfs::handle` has returned) | SAFE |
| A — `[G2MAP]` | syscall | **none** — the log is placed *after* `with_current_address_space_mut` returns, so the address-space `busy` flag is already released | SAFE. **Do not move it inside the closure.** |
| B — `[G2FALL]` | syscall | none | SAFE |
| C — `[G2ACQ]` | syscall | none *as specified* — `TMP_VMOS` and `TMP_FILES` are dropped first | SAFE. Logging *before* the drops would not deadlock (leaf `spin::Mutex`, IRQs enabled, `arch_serial_putc` takes no lock) but would hold the two mutexes that gate all tmpfs I/O across a multi-millisecond polled-UART write. Keep the drops. |
| D — `[G2MSF]` | syscall, **address space `busy` held** | AS `busy` (not RUN_QUEUE, not IRQ-off) | CONDITIONALLY SAFE — **flagged**. `lock_leader_address_space` (sched:1228) takes RUN_QUEUE only to flip `busy` and releases it before returning, so this is not the forbidden RUN_QUEUE context. But while `busy` is held, any page fault on this address space (including from another CPU) spins on `busy`; a fault *here* would self-deadlock. Printing integers cannot fault. **Never dereference `virt` or any user pointer at this site.** Default `MAP_SHARED_FRAMES_TRACE = false`. |
| E — `[G2IMP]` | syscall (recvmsg) | none after `drop(tbls)`; the caller already dropped the socket `conns` lock (servers/net:1830) | SAFE as specified |
| F — `[G2SUM]` | **IRQ — BSP timer tick** via `sched::timer_tick_irq` → `poll_deadline_tick` | `TMP_VMOS` via **`try_lock` only** | SAFE **only** under three constraints, all encoded above: (i) `try_lock`, never `lock()` — `TMP_VMOS` is taken from syscall context with interrupts enabled, so a blocking take from the tick self-deadlocks against a preempted holder on the same CPU; (ii) HHDM reads only — no user VA, therefore no demand fault, therefore no re-entry into the scheduler; (iii) no RUN_QUEUE, no `wake_poll`, no allocation. |
| G — `[G2UNMAP]` | syscall | none (before `with_current_address_space_mut`) | SAFE |

**Explicitly rejected sites** — do not instrument these while chasing Gap 2:

- `sched::handle_page_fault` / `mm::vmm::handle_user_page_fault` — the fault path
  is exactly where the frozen-vCPU hazard lives.
- `sched::service_poll_deadlines` and anything else called with RUN_QUEUE held.
- `handle_ftruncate`'s VMO branch (vfs:4707) *while* `TMP_VMOS` is held — if
  ftruncate tracing becomes necessary (row 6b), capture the values and print
  after the guards drop, exactly as Site C does.
- Any per-frame path in `drivers/src/drm_device_interface.rs`. Gap 2 is about
  what the compositor *reads*, not what it scans out; a per-flip trace would
  change the frame timing being observed.

---

## 6. Apply order

1. `mm/src/gap2.rs` (new) + `pub mod gap2;` in `mm/src/lib.rs`.
2. `servers/vfs/src/lib.rs`: `gap2_tmpfile_idx`, `gap2_kind_tag`,
   `vmo_debug_probe`; then Site C and Site E.
3. `kernel/src/syscall.rs`: Site M, Site A, Site B, Site F.3.
4. (`mm/src/vmm.rs` Site D only if needed.)
5. Flip `mm::gap2::ON = true`, build **release** (`./scripts/build-all.sh`),
   run the full COSMIC session on **aarch64** first (HVF, fast) with a persistent
   serial capture; let it run ≥120 s so ≥60 `[G2SUM]` samples land.
6. Walk the decision table. Repeat on x86_64 only if the aarch64 result is
   ambiguous.
7. Flip `ON` back to `false` before committing anything else.
