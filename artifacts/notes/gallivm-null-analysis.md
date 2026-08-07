# gallivm_create NULL root-cause analysis (M5e parked bug)

**Lane:** host-only, read-only. No QEMU, no git, no on-target runs. Docker/Alpine used
for source analysis, an instrumented libgallium build, and an strace baseline.

**Bug (recap):** llvmpipe (Alpine-built Mesa 25.3.6 + LLVM 19.1.4) crashes at llvmpipe
screen/context init on LeandrOS. `lp_texture_handle.c` precompiles bindless compute shaders;
each calls `gallivm_create()` with no null-check. After several **successful** `gallivm_create`
calls, one returns NULL → `lp_jit_init_cs_types` derefs NULL → fault. Deterministic. The
identical binary runs fine on Alpine-native Linux and under qemu-user.

---

## VERDICT (high confidence)

**Root cause is the LeandrOS kernel `sys_mmap` address-placement policy, not LLVM, not memory,
not W^X, not threads, not fds.** Specifically: LeandrOS honors a non-`MAP_FIXED` mmap **address
hint verbatim** and does **not** keep its global `MMAP_BUMP` allocator consistent with hinted
mappings, so a later `addr==0` allocation collides with a previously hint-placed JIT region and
fails hard with `ENOMEM` (no retry). musl's allocator then returns NULL, and
`gallivm_create`'s `CALLOC_STRUCT` (or an equivalent LLVM-internal allocation) returns NULL.

This is deterministic, count-based, and independent of total RAM — exactly the observed
signature — and it is LeandrOS-specific because Linux treats the hint as **advisory** and
relocates on conflict (proven below by strace).

---

## Why the failure must be a plain allocator-NULL (narrowing the surface)

The Alpine build compiles the **MCJIT** init path, `lp_bld_init.c` (NOT the ORC path
`lp_bld_init_orc.cpp`):
- `meson.options`: `llvm-orcjit` defaults **false**.
- `meson.build:1758`: `llvm_with_orcjit = get_option('llvm-orcjit') or not llvm_has_mcjit`;
  `llvm_has_mcjit` is true for both `aarch64` and `x86_64`. ⇒ `llvm_with_orcjit = false`.
- Build log confirms: `Compiling C object ... gallivm_lp_bld_init.c.o` (the `.c`, MCJIT).

In `lp_bld_init.c::init_gallivm_state()` on **LLVM 19**, the per-call failure points reduce to
almost nothing:
- `lp_build_init()` — one-time, guarded by the `gallivm_initialized` static; returns `true`
  immediately after the first call. Not a per-call failure.
- `context->ref` — the LLVMContext is shared and valid after first use (proven: first several
  succeed).
- `create_pass_manager()` → `lp_passmgr_create()` — on LLVM 19 `USE_NEW_PASS==1`, so this is a
  **no-op that always returns true** (`lp_bld_passmgr.c`).
- `lp_get_default_memory_manager()` = `new llvm::SectionMemoryManager()` — a **trivial ctor**
  that maps nothing at construction (mapping is lazy, at section-emit time).

What remains are pure allocations: `CALLOC_STRUCT(gallivm_state)` (musl calloc),
`LLVMModuleCreateWithNameInContext`, `LLVMCreateBuilderInContext`, `LLVMCreateTargetData`
(string parse). On genuine OOM, LLVM's C++ `new` / `safe_malloc` **aborts** (report_bad_alloc),
it does not silently return NULL. The observed behavior is a **clean NULL deref, not an abort**
— so the failing allocation is one that *returns* NULL rather than aborting, i.e. musl's own
malloc/calloc returning NULL. The cleanest such site is `CALLOC_STRUCT(gallivm_state)`
(`gallivm_create`, `lp_bld_init.c:344`), which returns NULL → `gallivm_create` returns NULL
without even entering `init_gallivm_state`. musl `calloc` returns NULL when it needs to grow its
arena via `mmap` and that `mmap` fails.

**So the question collapses to: why does an anonymous `mmap` fail on LeandrOS after N JIT
allocations?**

---

## The kernel mechanism (H1)

`kernel/src/syscall.rs::sys_mmap`, anonymous path:

```
addr==0            → virt = MMAP_BUMP.fetch_add(len)     // monotonic global bump, never reclaimed
addr!=0, !FIXED    → virt = addr                         // HINT USED VERBATIM
MAP_FIXED          → virt = addr                         // unmap-then-map
... then map_lazy(virt, len):
      returns false on ANY overlap with an existing region  (mm/src/vmm.rs:358-362)

on map_lazy==false:
   addr!=0, !FIXED  → retry once on MMAP_BUMP   (fallback exists)   // syscall.rs:1441-1445
   addr==0          → return -12 (ENOMEM)        (NO retry)         // syscall.rs:1445 else
```

Key facts:
1. `MMAP_BUMP` (`syscall.rs:36`) is a single global `AtomicUsize` starting at `0x4000_0000`.
   `USER_SPACE_END = 0x8000_0000_0000` — so address-space *exhaustion* is impossible (128 TiB).
2. A **hinted** mapping (`addr!=0`, non-FIXED) that succeeds is placed at `addr` and does
   **NOT advance `MMAP_BUMP`**.
3. `map_lazy` (`mm/src/vmm.rs:340`) returns `false` on any overlap; the `regions` Vec itself
   grows dynamically (`push`), so there is **no fixed VMA cap** — the failure is placement, not
   a slot limit.

LLVM's `SectionMemoryManager` (RTDyld/MCJIT backend — `USE_JITLINK` is only riscv/loong/win, so
x86_64+aarch64 use RTDyld) allocates each JIT section via
`Memory::allocateMappedMemory(..., NearBlock, ...)`, which passes a hint
`addr = NearBlock->base() + NearBlock->allocatedSize()` (page-rounded) **without** `MAP_FIXED`.
That hint equals the end of the previous JIT block — which equals the current `MMAP_BUMP` value
(the previous `addr==0` section advanced the bump to exactly there).

**The collision:**
- Section A (first in a memory group): `Near=null` → `addr==0` → `virt = bump = B`. Placed
  `[B, B+a)`; bump → `B+a`.
- Section B: `Near=A` → hint `= B+a` → placed **verbatim** at `[B+a, …)`. **Bump stays `B+a`.**
- Now `MMAP_BUMP` points *into* an occupied region. The **next `addr==0` mmap** — musl growing
  its arena to satisfy a later `gallivm_create`'s `CALLOC_STRUCT`, or a new memory group's first
  section — gets `virt = bump = B+a`, which **overlaps** Section B → `map_lazy==false` →
  `addr==0` has **no retry** → `-12 ENOMEM` → musl `mmap` = `MAP_FAILED` → `calloc` = NULL →
  `gallivm_create` returns NULL.

The "after several successes" count is simply *when the next `addr==0` mmap lands after enough
hinted JIT sections have stacked above the stale bump* — a fixed function of the (fixed) shader
precompile workload ⇒ deterministic, and independent of total RAM.

### strace proof of the divergence (host kernel relocates hints; LeandrOS does not)

Native aarch64 llvmpipe smoke under strace (`diag2/strace-aarch64.raw`, full run PASSED,
`GL_RENDERER=llvmpipe (LLVM 19.1.4, 128 bits)`):

```
mmap(0xffffa007a000, 4096, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0) = 0xffffa0078000
mmap(0xffffa00fc000, 4096, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0) = 0xffffa00fa000
mmap(0xffffa0087000, 4096, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0) = 0xffff9ff5d000
... (8 such non-FIXED hinted mmaps)
```

- LLVM **does** issue non-`MAP_FIXED` mmaps with a **non-zero addr hint** (the LeandrOS-divergent
  path is exercised).
- The Linux kernel **relocates every one**: the returned address ≠ the requested hint. Linux
  treats a non-FIXED hint as advisory and never returns an overlapping mapping.
- LeandrOS `sys_mmap` uses `virt = addr` verbatim and only falls back to the bump on overlap —
  and crucially leaves the bump stale, setting up the later fatal `addr==0` collision.
- JIT execute pages: 24 `PROT_EXEC` mmaps + 8 `mprotect`→`PROT_EXEC` (RW→RX); `memfd_create`=1.
  Confirms MCJIT uses **anonymous** mmap + mprotect for code (no fd per section) ⇒ fd exhaustion
  is not the mechanism.

### Instrumented-lib confirmation on host

The GVDIAG libgallium (below) run against the host smoke: **16 `gallivm_create` calls, all
succeed**, no FAIL/PROBE lines. The very first module is `name=jit_size_function` — exactly the
build the M5e note fingered as returning NULL on-target. On the host (proper mmap relocation) it
succeeds; on LeandrOS one of these 16 will log the NULL + errno + probe.

---

## Ranked hypotheses + on-target falsification (one run each)

Drop in the GVDIAG libgallium (staged, see below), run the pure-C kmscube/llvmpipe repro, and
grep the serial for `[GVDIAG]`. The single run discriminates all of these.

| # | Hypothesis | Depends on | Expected GVDIAG / kernel signature | Confidence |
|---|-----------|-----------|-----------------------------------|-----------|
| **H1** | **MMAP_BUMP hint/bump placement collision** (kernel honors non-FIXED hint verbatim, leaves bump stale, `addr==0` overlap → hard ENOMEM) | `sys_mmap` anon path; LLVM SectionMemoryManager Near-hints | Failing `gallivm_create#N` logs `CALLOC_STRUCT RETURNED NULL … errno=12` (or an `init#N` step `=NULL errno=12`), then `PROBE(...) mmap(NULL,4096)=0xffffffffffffffff errno=12` and/or `mmap(hint…)=…errno=12`. Optional 1-line kernel log in `sys_mmap` shows an `addr==0` request whose bumped `virt` overlaps a prior region, returning -12, immediately before. | **PRIMARY / high** |
| H2 | fd exhaustion (EMFILE — 128-cap / eventfd-timerfd pool) | per-process fd table; `net`/`vfs` fd leaks | PROBE errno = **24 (EMFILE)**, not 12; openat storm in trace. MCJIT uses anon mmap (no fd/section); the standalone kmscube repro is isolated from cosmic-comp's fd leak. Largely ruled out. | low |
| H3 | Host-CPU feature detection via `AT_HWCAP=0` / absent `/proc/cpuinfo` (`getHostCPUName`/`Features`) | auxv, procfs | Would corrupt **codegen at compile** (EngineBuilder `selectTarget`), one-time + cached; the first several JITs succeed (proven). Cannot produce a per-call `gallivm_create` NULL. GVDIAG shows several `init#N OK` before any failure ⇒ init path is not feature-gated. | low (not this crash) |
| H4 | Genuine heap OOM / `RLIMIT_AS` / `RLIMIT_DATA` | rlimits, total RAM | Ruled out: identical crash at 8 GiB; deterministic count, not size-correlated. PROBE would show low RSS with mmap failing. | very low |
| H5 | Kernel-side exhaustion of `regions.push`/`lazy_pages` (slab) | kernel heap | Would **panic the kernel**, not deliver a clean userspace NULL. Distinguish by absence of any kernel panic and PROBE errno=12 from a *successful* syscall return. | very low |

**Cheapest single confirmation:** the GVDIAG PROBE lines. If `mmap(NULL,4096)` returns
`MAP_FAILED errno=12` at the failing `gallivm_create`, H1 is confirmed and H2/H4 are excluded in
one run. Add one temporary `serial_print` in `sys_mmap`'s `addr==0 → Some(false) → -12` arm to
capture the colliding `virt`/bump for the record.

---

## Recommended fix direction (for the tree wave — NOT done here)

The bug is entirely in `kernel/src/syscall.rs::sys_mmap` + the `MMAP_BUMP` policy. Options, in
order of preference:

1. **Treat non-FIXED hints as advisory (like Linux) and keep the bump consistent.** Simplest
   correct fix: whenever a mapping is placed at or above `MMAP_BUMP` (hinted *or* fixed), advance
   `MMAP_BUMP` past its end; and when `addr==0` overlaps, **retry** (loop re-bump until
   `map_lazy` succeeds) instead of returning -12. This mirrors the retry that already exists for
   the `addr!=0` path.
2. **Ignore the hint for non-FIXED mmap and always allocate from the bump.** LLVM does not
   require the hint honored (proven — Linux relocates it). Caveat: JIT code then lands wherever
   the bump is; since the bump is contiguous this keeps sections close, so x86_64 small/medium
   code-model relative calls remain in range. Safe.
3. **Make the bump a real gap-finding allocator** (scan `regions` for the next free hole ≥ len).
   Most robust, most code.

Any of these removes the hard `addr==0`/hint collision. Recommend (1): smallest diff, preserves
hint locality, removes the only failure edge. Re-test: the GVDIAG lib should then log all
`gallivm_create#N OK` and llvmpipe should reach first draw.

Note: this same `sys_mmap` placement policy is a latent hazard for **any** JIT/allocator that
passes mmap hints (not just llvmpipe) — worth fixing regardless of the llvmpipe milestone.

---

## Enumerated `init_gallivm_state` steps and their OS inputs (reference)

MCJIT path, `lp_bld_init.c`, LLVM 19. "OS input" = what a failure would consume/depend on.

| Step | Call | Can return NULL? | OS input |
|------|------|------------------|----------|
| lp_build_init | one-time, `gallivm_initialized` guard | no (returns true after 1st) | LLVMLinkInMCJIT, env opts — one-time |
| context | `context->ref` | shared/valid | none |
| module_name | `MALLOC` | not fatal (unchecked) | musl malloc |
| module | `LLVMModuleCreateWithNameInContext` | aborts on OOM, else valid | LLVMContext BumpPtrAllocator → malloc → **mmap arena growth** |
| builder | `LLVMCreateBuilderInContext` | aborts on OOM | malloc |
| memorymgr | `new SectionMemoryManager` | aborts on OOM (trivial ctor, no mmap) | malloc (tiny) |
| target | `LLVMCreateTargetData(layout)` | parses const string; fatal-errors on bad layout (const ⇒ never) | none |
| passmgr | `create_pass_manager`→`lp_passmgr_create` | **no-op, always true (LLVM19 NewPM)** | none |
| **(caller)** | `CALLOC_STRUCT(gallivm_state)` in `gallivm_create` | **YES — musl calloc returns NULL** | **musl calloc → mmap arena growth ← the failing syscall** |

Host CPU / feature detection (`getHostCPUName`, `getHostCPUFeatures`, MAttrs) and the
`ExecutionEngine`/TargetMachine creation happen in `init_gallivm_engine` →
`lp_build_create_jit_compiler_for_module`, called later from `gallivm_compile_module`, **not** in
`init_gallivm_state`. They cannot cause a `gallivm_create` NULL and are proven-good by the first
several successful JITs.

---

## Staged deliverables (`llvmpipe-lane/diag2/`)

- `gvdiag.patch` — unified diff against
  `src/gallium/auxiliary/gallivm/lp_bld_init.c` adding unbuffered `write(2,…)` `[GVDIAG]`
  logging of every `gallivm_create`/`init_gallivm_state` allocation + errno, plus an allocator
  `PROBE` (`mmap(NULL,4096)`, hinted `mmap`, `malloc`) at each failure point.
- `lp_bld_init.c.instrumented` / `lp_bld_init.c.orig` — patched and pristine copies.
- `build-diag2.sh` — Alpine 3.21 build recipe (mirrors `build-in-alpine.sh` exactly:
  llvmpipe+softpipe, shared LLVM 19; then patchelf `libc.musl→libc.so`) ⇒ a **soname/ABI-identical
  drop-in** for the crashing ship set. Only `libgallium-25.3.6.so` changes.
- `strace-baseline.sh` + `strace-aarch64.raw` — the successful host JIT-era syscall baseline.
- `stage-diag2-aarch64/usr/lib/libgallium-25.3.6.so` — **BUILT + validated** (NEEDED `libc.so`,
  soname `libgallium-25.3.6.so`; host smoke PASSED against it, GVDIAG fires: 16 `gallivm_create`
  all OK on host).
- `stage-diag2-x86_64/usr/lib/libgallium-25.3.6.so` — built by `build-diag2.sh x86_64` (emulated).

**On-target use (tree wave):** swap only `libgallium-25.3.6.so` in the llvmpipe ship set with the
diag2 one (all other libs/deps unchanged), run the pure-C kmscube/llvmpipe repro, capture serial,
grep `[GVDIAG]`. The failing `gallivm_create#N` line + its `PROBE` errno pinpoint the step and
confirm/deny H1 in a single run. Prefer x86_64 (zero kernel risk); aarch64 is equivalent for the
diagnostic (bug is arch-independent — kernel mmap policy).

---

## Checkpoint
- 2026-07-24: Root-caused to `sys_mmap` non-FIXED-hint / `MMAP_BUMP` placement collision (H1,
  high confidence). Enumerated init steps; ranked hypotheses + 1-run falsification. Built +
  validated GVDIAG aarch64 libgallium (host smoke PASS, instrumentation fires); x86_64 building.
  strace baseline captured (LLVM issues non-FIXED hinted mmaps; Linux relocates, LeandrOS does
  not). No on-target runs. Recommended kernel fix: advisory hints + bump-consistency + addr==0
  retry.
