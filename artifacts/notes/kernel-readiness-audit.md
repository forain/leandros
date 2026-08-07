# Kernel Readiness Audit — llvmpipe JIT, busy-poller census, image budget

Host-only read-only lane. Repo `/Users/forain/code/leandros` untouched.
Evidence is `file:line` against the tree at HEAD (main, 06defe1 + uncommitted syscall.rs instrumentation).

---

## TASK 1 — llvmpipe JIT readiness: anon `mmap(RW)` → `mprotect(→RX)`

**VERDICT: x86_64 = READY. aarch64 = NEEDS-FIX (I-cache/CTR EL0 trap; one boot-time SCTLR change).**

### The mmap→write→mprotect flow is supported (both arches)

- **`sys_mmap`** `kernel/src/syscall.rs:1388`. Anonymous `PROT_READ|PROT_WRITE` mmap → `map_lazy` (`:1425-1442`) creates a lazy VMA. Pages fault in RW as llvmpipe writes code. This is the normal case.
- **`sys_mprotect`** `kernel/src/syscall.rs:2506` → `AddressSpace::mprotect` `mm/src/vmm.rs:995`. The flip **RW → R+X works**:
  - It splits VMAs straddling the range (`split_at` `mm/src/vmm.rs:761`, `mprotect` calls it at `:1011-1012`), so a **middle-split / sub-range** mprotect changes only the named pages — the exact machinery the dynamic loader's RELRO pass already relies on. llvmpipe protecting one code section inside a larger arena is the same shape and is handled.
  - It **re-installs every already-backed page** with the new flags: lazy VMAs at `:1030-1048`, eager at `:1049-1055`, then `tlb_shootdown_all()` at `:1059`. Because llvmpipe writes all code (faulting the pages in RW) *before* mprotect, every code page is backed and gets remapped executable immediately — no later fault, no W^X re-check on the exec path.

### W^X policy — RWX is rejected, RW→RX is not

- `sys_mmap` rejects simultaneous W+X: `kernel/src/syscall.rs:1404-1406` (`EINVAL`).
- `mprotect` rejects simultaneous W+X: `mm/src/vmm.rs:996` (`return false` → `EINVAL`).
- **Impact for llvmpipe:** the standard SectionMemoryManager path (allocate RW, write, finalize→RX) is fine. **RWX directly (LLVM RuntimeDyld's occasional `mmap(PROT_READ|WRITE|EXEC)`) is BLOCKED.** Mesa's llvmpipe uses `mmap(RW)`+`mprotect(RX)` via LLVM's default `Memory::AllocateRWX`→sections path, so this is normally not hit — but if a build enables the RWX-in-one-shot allocator it will get EINVAL and fall over. Note it as a compatibility caveat, not a blocker.

### PROT_EXEC → PTE translation

- **x86_64** `arch/x86_64/src/paging.rs:380-391`: `NO_EXECUTE` (NX, bit 63) is set **only when EXECUTE is not requested** (`:391`). RX pages are executable; W and X never coexist (blocked upstream). x86 has coherent I/D caches — `arch_flush_cache_range` is a no-op (`arch/x86_64/src/lib.rs:23`) and `__clear_cache` is a no-op on x86. **Nothing to fix.**
- **aarch64** `arch/aarch64/src/paging.rs:180-192` (`translate_flags`): when EXECUTE is set, neither UXN nor PXN is applied → page is executable at EL0. Correct for JIT. (Minor security nit, not a blocker: line `:187` clears **both** UXN and PXN for an executable *user* page, so the kernel can also execute user JIT code — PXN should stay set on EL0-executable pages. Flag for hardening, unrelated to llvmpipe correctness.)

### aarch64 I-cache / D-cache coherency — THE fix

JIT'd code is written through the D-cache and then fetched through the I-cache; on aarch64 the two are not coherent, so between write and execute you must run `DC CVAU` (clean D to PoU) + `DSB` + `IC IVAU` (invalidate I) + `DSB` + `ISB`. On Linux this is done **in userspace** by `__clear_cache` / `__builtin___clear_cache` (which LLVM's `InvalidateInstructionCache` calls after finalizing), and Linux enables that by setting `SCTLR_EL1.UCI`. LeandrOS does neither:

1. **The kernel does no I-cache maintenance on mprotect.** `mm/src/vmm.rs:995-1061` only remaps PTEs + `tlb_shootdown_all`. The only cache helper, `arch_flush_cache_range` (`arch/aarch64/src/lib.rs:14-22`), is **dead code** (no callers anywhere — grep across the tree finds only the def and the `extern` decl at `kernel/src/main.rs:102`) **and incomplete** (it issues `dc cvau` only — no `ic ivau`), so even if wired it would not make JIT code coherent.

2. **`SCTLR_EL1.UCI` (bit 26) and `SCTLR_EL1.UCT` (bit 15) are never set.** Grep for UCI/bit-26 SCTLR writes returns nothing; the only SCTLR writes are the reset value `0x30d0_0800` (`kernel/src/entry_aarch64.s:131-133`, `arch/aarch64/src/smp.rs:130`) and `orr sctlr,#1` to enable the MMU (`entry_aarch64.s:220`, `smp.rs`). Bit 26 and bit 15 are 0. Consequently, when llvmpipe's `__clear_cache` runs at EL0 it will:
   - `MRS ctr_el0` (to read cache line size) → traps (UCT=0),
   - `DC CVAU` / `IC IVAU` → trap (UCI=0),
   with **ESR EC = 0x18** ("trapped MSR/MRS/system instruction"). The EL0 sync handler `arch/aarch64/src/exception.rs:142` decodes only `ec==0x15` (SVC) and `ec==0x24/0x20` (aborts); **EC 0x18 is unhandled → the process is killed** (falls through to the "try to deduce"/print-ESR branch at `:184-200`). **llvmpipe on aarch64 crashes the moment it finalizes JIT code.**

3. **No `cacheflush`/`__clear_cache` syscall exists** in the table (grep empty), so relibc/compiler-rt cannot route around the trap via a syscall either.

**Why TCG hasn't shown this:** TCG re-translates guest code from memory, so it masks the *incoherency* — but TCG still honors `SCTLR.UCI` and would still trap the EL0 cache ops. On HVF (Apple Silicon) both the trap and real incoherency bite. Either way the fix is required before llvmpipe runs.

**FIX (exact location, mirrors Linux):** set `SCTLR_EL1.UCI` (bit 26, `0x0400_0000`) **and** `SCTLR_EL1.UCT` (bit 15, `0x8000`) when the MMU is enabled, on **every** core:
  - BSP: `kernel/src/entry_aarch64.s` — add `orr` of `(1<<26)|(1<<15)` alongside the `orr x4,x4,#1` MMU-enable at `:219-221` (and the same in the direct-boot path there), or fold into `mmu::enable_identity` (`arch/aarch64/src/mmu.rs`).
  - Secondary CPUs: `arch/aarch64/src/smp.rs` — the SCTLR the trampoline installs (`:169-171`) must carry the same bits (or set them in the per-CPU SCTLR value at `:89`/`:130`).
  This lets stock `__clear_cache` (in the Alpine-built llvmpipe deps) do the real `DC CVAU`+`IC IVAU`+`CTR_EL0` read at EL0 — correct under HVF, harmless under TCG. No kernel cache-maintenance code and no syscall needed once UCI/UCT are on. (Optionally also fix/retire the dead `arch_flush_cache_range`, but it is not on the critical path.)

---

## TASK 2 — remaining busy-poller census

The scheduler idle path is correct: when `pick_next` returns nothing it executes `sti;hlt` / `wfi` (`sched/src/lib.rs:1445-1451`). Therefore a ~100% idle floor means **a task that is perpetually `Ready`** — a `yield_now` loop with no blocking wait keeps itself runnable so the CPU never reaches `hlt`/`wfi`.

Every `yield_now` is a full switch to the scheduler context (`sched/src/lib.rs:1094`); a task that loops on it without first marking itself `Blocked` spins a core. The already-committed fix pattern (three-phase block on the poll wait-channel) is: `block_on_poll_prepare()` → `register_poll_deadline(now+N)` → re-check condition → `block_on_poll_commit()` (see `kernel/src/syscall.rs:6333` `poll_block`, and the fixed stdin path at `:3566-3573`).

### (a) Busy-spin loops that should block — RANKED by CPU cost

| # | Site | Cost | Wait-condition / wake edge | Fix shape |
|---|------|------|----------------------------|-----------|
| **1** | **`servers/init/src/lib.rs:2601`** `event_loop()` | **CRITICAL — this is the ~100% floor.** PID 1's top-level loop is `loop { …heartbeat every 100 ticks…; yield_now("init_idle") }` with **no blocking wait**. init is always `Ready`, so one core is pinned forever, compositor or not. Called every scheduler cycle — the hottest possible spin. | init needs to wake on (a) a child exiting (to reap orphans as PID 1) and (b) the 1 s heartbeat. The child-exit wake edge is the same one `wait4` now uses. | Three-phase block on the poll/child wait-channel with `register_poll_deadline(now+1)` (drops to ~100 Hz) or `now+100` for the pure-heartbeat cadence; wake on the child-exit edge. This single fix removes the idle floor. |
| **2** | `kernel/src/syscall.rs:5830` `net_blocking_op` | High — every blocking socket `send`/`recv`/`connect`/`accept` on a not-yet-ready socket busy-loops (`handle→EAGAIN→irq_window→yield_now`). Hot for D-Bus/pipewire/any blocking-socket client in the session; per-syscall-retry tight loop. | Wait-condition: net server has data/space/peer for the fd. Wake edge exists: the net server already wakes pollers (`unblock_port`/`wake_poll`) — epoll/poll on the same fd block correctly via it. | Mirror the three-phase block: prepare→`register_poll_deadline(now+1)`→re-probe the socket op→commit. Same edge poll already uses. |
| **3** | `kernel/src/syscall.rs:3649` `sys_read_sock`; `:3675` `sys_read_vfs` | High-ish — blocking `recv()` on an empty socket and blocking pipe/file read on `-EAGAIN` busy-loop. Note the **stdin** sibling in the same function was already converted to three-phase (`:3566-3573`); these two were not. | Socket: peer write / close. Pipe: writer write / close. Both drive the poll wait-channel already. | Same three-phase block as #2; re-probe via the existing `probe_fd_events`/net-recv check. Also the write-side twins `:3450` `sys_write_sock` / `:3467` `sys_write_vfs` (lower cost — writes rarely block). |
| **4** | `sched/src/futex.rs:67-81` `futex_wait` (timed path only) | Medium — the **deadline** branch polls the futex word every tick with `yield_now("futex_wait_timed")` until value-change/timeout. The untimed path (`:84+`) blocks correctly. A multithreaded compositor using `parking_lot`/condvar *timed* waits re-arms this repeatedly. | Value change at `uaddr` / `futex_wake`. No timed registration exists today (documented at `:55-66`). | Add a deadline-aware blocked wait (register the waiter + a tick-driven deadline waker), so timed futex waiters sleep instead of poll. Larger change; do after #1–#3. |

### (a-conditional) Active only in specific modes — fix opportunistically

- `servers/init/src/lib.rs:1086` `readline` (built-in shell), `:263` `vfs_read_blocking`, `:2041` `read` builtin — poll the `(*IO).read_byte` hook with `yield_now`. Pins a core **only while init's built-in shell/getty is the active console waiting for a line**. In the compositor path the session replaces init's shell, so usually cold; but a getty idling at a prompt on a VT would spin. Fix: have `read_byte` block (route through the already-blocking `sys_read_stdin` console path) or three-phase-block the readline loop.
- `servers/init/src/lib.rs:1393` `cmd_sleep` — bounded by the sleep duration; only during a `sleep N` builtin. Low.

### (b) Legitimate / bounded — leave as-is

- `sched/src/lib.rs:795` `block_on_port_commit` `yield_now` — **part of** the three-phase IPC block (task is marked `Blocked` before the yield); not a spin. This is why servers idle at `hlt`, not 100%.
- `sched/src/lib.rs:1197` address-space busy-lock retry, `:1242` AP park-until-`SCHED_ONLINE` (one-shot boot), `:1746` — short contention/boot spins.
- `sched/src/clone.rs:528` `vfork_wait` — brief, until child execs/exits.
- Driver register polls with bounded/one-shot waits, active only during device I/O: `drivers/src/virtio_blk.rs:190`, `virtio_net.rs:167`, `virtio_gpu.rs:491/746`, `sdhci.rs` (several), `snd.rs:410`, `drivers/usb/src/xhci/mod.rs` (several), `hub.rs:27`. `servers/pipewire/src/lib.rs:109` — bounded drain wait, only during audio streaming.
- `servers/vfs/src/lib.rs:3570` `fcntl_setlkw`, `:3611` `flock` — spin only under **active** advisory-lock contention (a second holder). Rare/cold; correct-but-not-ideal. Convert to a blocked wait if lock contention ever shows up.

### (c) Cold / unreachable terminals

- `kernel/src/main.rs:615/631`, `servers/libc-shim/src/lib.rs:53`, `sched/src/lib.rs:1577` (post-`exit`), and various panic paths: `loop { spin_loop() }` catch-alls after a dead/halted task. Reached only on fatal error or a parked slot; not an idle-floor contributor.

**Bottom line:** fix **#1 (`init` event_loop)** and the ~100% idle floor with one compositor disappears. #2–#4 are the next tier (socket/pipe blocking I/O and timed futex) and matter once the session does real client traffic.

---

## TASK 3 — image size budget for llvmpipe (+~150 MB/arch)

**VERDICT: FITS automatically. There is NO fixed size constant to bump.**

### How sizing works (`scripts/mkfs-f2fs-populated.py`)

The image is **dynamically sized to ~2× its content** (the margin logic per commit 36218c3, now embodied at `:539-553`):
- `required_blocks` accumulates a base `4096+100` plus, for each **distinct host path** (hardlinks counted once — `:520-538`), `k + 1 + (k+1018)//1019` blocks (data + inode + direct-node blocks), then `+2*BLOCKS_PER_SEG` runtime reserve (`:542`).
- `segs = ceil(required_blocks/512)`; **`total_segs = max(32, segs*2)`** (`:551-552`) — the ×2 gives ~50 % free; the min floor is 32 segs = 64 MB.
- `total_blocks = total_segs*512`, image bytes = `total_blocks*4096`.

`build-all.sh:363-364` invokes it per arch and then `cp`s data0→data1 (vdb root + vdc /data), so **both** images grow and disk cost doubles per arch.

### Current vs. added

| | aarch64 | x86_64 |
|---|---|---|
| Current `f2fs-data0-*.img` | 1,228,931,072 B (1.14 GB) | 1,254,096,896 B (1.17 GB) |
| llvmpipe deps to add (`deps-<arch>/`) | 147 MB (`libLLVM.so.19.1` = 147.9 MB + libstdc++ 3.2M, libxml2 1.3M, libzstd 0.7M, liblzma 0.3M, libgcc 0.2M) | 162 MB |
| Content growth | ~+147 MB → ~+73 segs required | ~+162 MB → ~+80 segs required |
| **Projected image after ×2 margin** | **~1.43 GB (+~290 MB)** | **~1.48 GB (+~320 MB)** |

The `mkfs` recomputes `total_segs` from content every run, so the image **auto-grows** to absorb the libs. No constant changes.

### Metadata / inode headroom — enormous margin

- **NAT** (node/inode IDs): fixed `SEG_CNT_NAT=2` segments = 1024 blocks × (4096/9) = **~465,920 max nids** (`:35`, `:75`, `write_nat_entry:173`). llvmpipe adds ~10 files (≈10 inode nids + a few dozen direct-node nids for `libLLVM`'s ~36 k data blocks); the icon set adds up to 681 files (`notes/m6-icons-manifest.md`) = 681 nids. Both are negligible against 465 k.
- **SIT** (per-segment info): fixed `SEG_CNT_SIT=1` segment = 512 blocks × (4096/74) = **~28,160 segment entries** → hard image ceiling ≈ **56 GB** (`:36-37`, `:910-913`). Projected ~1.5 GB images use ~750 of 28,160 entries. Huge margin.
- Directory dentry blocks spill across multiple 4 KB blocks automatically (`build_dentry_blocks:88`), so a fat `/usr/lib` or an icons dir is fine.

### The actual integration action (not a size constant)

The llvmpipe `.so`s are **not referenced by the script today**. At M7, add them to the packing lists exactly like the existing GL/input ship sets (`:369-414`):
- Append `libLLVM.so.19.1`, `libstdc++.so.6(.0.33)`, `libxml2.so.2(.13.9)`, `libzstd.so.1(.5.6)`, `liblzma.so.5(.8.3)`, `libgcc_s.so.1` (plus the actual llvmpipe/gallium swrast `.so` from `llvmpipe-lane/stage-<arch>/usr/`) to **`usr_lib_files`** (→ `/usr/lib`, `add_files_to_dir(16, …)` at `:820-821`), pointing at `~/code/leandros-artifacts/llvmpipe-lane/stage-<arch>/usr/lib` (or `deps-<arch>/`).
- No edit to any size/segment constant. Optionally, since libLLVM is read-only and never written at runtime, the ×2 margin at `:552` could be trimmed to save disk — but leaving it is safe and matches the documented "margin scales with content" policy (`:548-550`).

**One watch item:** deps land in both data0 and data1 (the `cp` at `build-all.sh:364`) and across two arches → roughly **+1.2 GB of host disk** for the four images combined. Functionally fine; just disk usage.

---

## Summary of verdicts

1. **llvmpipe JIT — x86_64 READY; aarch64 NEEDS-FIX.** RW→RX `mprotect` is fully supported (VMA split + backed-page remap, `mm/src/vmm.rs:995`); RWX-in-one-shot is rejected (W^X, caveat only). aarch64 blocker: `SCTLR_EL1.UCI`(bit 26)+`UCT`(bit 15) are never set, so EL0 `__clear_cache`/`ctr_el0` traps (EC 0x18) into an unhandled path (`arch/aarch64/src/exception.rs:142`) and kills the process; kernel does no I-cache maintenance on mprotect and has no cacheflush syscall. Fix = OR those SCTLR bits in at MMU-enable on all cores (`kernel/src/entry_aarch64.s:219-221`, `arch/aarch64/src/smp.rs:169`).
2. **Busy-poller census — the 100% floor is `servers/init/src/lib.rs:2601`** (`event_loop`, PID 1 unconditional `yield_now`). Next tier: `net_blocking_op` (`syscall.rs:5830`), `sys_read_sock`/`sys_read_vfs` (`:3649`/`:3675`), timed `futex_wait` (`sched/src/futex.rs:67`). All fixable with the committed three-phase block pattern.
3. **Image budget — FITS, no constant to change.** `mkfs-f2fs-populated.py:551-553` auto-sizes to ×2 content; +147/162 MB grows each image ~+290/320 MB (to ~1.43/1.48 GB), far under the ~56 GB SIT ceiling and ~465 k nid capacity. Integration = add the `.so`s to the `usr_lib_files` list (`:820`), not a size constant.
