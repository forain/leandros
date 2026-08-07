# K3 — Dynamic-binary support in the ELF loader (PT_INTERP, ET_DYN bias, auxv)

Target: LeandrOS kernel, both x86_64 and aarch64. Read-only design; no code changed.
All musl claims below are verified against the musl 1.2.5 source tree at
`/Users/forain/.claude-forain/jobs/afde2e74/tmp/musl-dynamic/src/musl-1.2.5` (not inferred).

---

## 0. Executive summary

- musl **self-relocates**. Neither the static-PIE path nor the PT_INTERP path needs a
  relocation processor in the kernel. Verified from `crt/rcrt1.c`, `ldso/dlstart.c`,
  `arch/{x86_64,aarch64}/crt_arch.h`. The kernel's entire job is: map segments at a bias,
  set the correct auxv, and jump to the right entry.
- The loader change is small and arch-independent (bias parameter + PT_INTERP extraction).
- The **real work is in the VMM**, not the loader: `mm::vmm::AddressSpace` cannot split a VMA.
  This breaks two things the dynamic loader relies on — `mmap(MAP_FIXED)` hole-punch overlays
  (`unmap_range`) and sub-range `mprotect` for RELRO. These are the riskiest, load-bearing changes.
- Back-compat is trivially preserved: `ET_EXEC` + no PT_INTERP takes the current path with `bias = 0`.

---

## 1. Current state (verified, file:line)

### Loader — `elf/src/lib.rs`
- `parse_ehdr` already accepts **both** ET_EXEC and ET_DYN (`:116`), rejects other types.
- `load()` (`:174`) and `load_lazy()` (`:321`) map every PT_LOAD at **literal `p_vaddr`**
  (`:198`, `:227`, `:349`, `:368`) — **no load bias**.
- `load_base = vaddr.wrapping_sub(foffset)` of the first PT_LOAD (`:207`, `:355`); `ElfInfo.phdr_va = load_base + phoff` (`:307`, `:399`).
- Entry is raw `e_entry` (`:306`, `:398`). **No relocations** are applied anywhere.
- Consequence (S1): a static-PIE (first PT_LOAD `p_vaddr = 0`) maps onto the null page and `e_entry`
  is a tiny offset → immediate fault. ET_DYN is *accepted* but *unusable* today.
- PT_INTERP is never inspected (the doc-comment at `:4` says it is "ignored"; the code neither
  reads it nor rejects it). There is no `PT_INTERP` constant.

### Exec + stack/auxv — `kernel/src/syscall.rs`
- `sys_execve` (`:2660`). ELF source resolution: demand-paged `open_exec_header`+`load_lazy`
  for f2fs-backed files (`:2684`, `:2803`), else eager `read_file_from_vfs`/initrd + `load` (`:2814`).
- `open_exec_header` (`:2578`) reads the ELF+phdr table into a buffer (grows to `needed`,
  capped at `EXEC_HEADER_MAX`). **It does not guarantee the PT_INTERP *string* is in the buffer** —
  it sizes to `phoff + phentsize*phnum`, and the interp path lives at `PT_INTERP.p_offset` (usually
  right after the phdrs, so usually already covered, but not guaranteed).
- User stack: mapped eagerly `[USER_STACK_TOP - USER_STACK_SIZE, USER_STACK_TOP)` (`:2830`),
  `USER_STACK_TOP = 0x0000_7fff_ffff_f000` (`:201`), `USER_STACK_SIZE = 64 pages = 256 KiB` (`:203`).
- Stack frame built at `:2854`–`:2982`. Current auxv (`:2951`–`:2965`), **13 pairs, hardcoded count at `:2896`**:
  AT_PHDR(3), AT_PHENT(4), AT_PHNUM(5), AT_RANDOM(25), AT_PAGESZ(6), AT_UID(11), AT_EUID(12),
  AT_GID(13), AT_EGID(14), three private Leandros ports (256/257/258), AT_NULL(0).
  **Missing: AT_ENTRY(9), AT_BASE(7), AT_SECURE(23), AT_HWCAP(16), AT_CLKTCK(17), AT_EXECFN(31).**
- AT_RANDOM data = 16 bytes derived from `ticks()` (`:2976`–`:2981`) — adequate for musl SSP.
- `replace_address_space(new_as, pt_root, heap_start, entry, user_sp)` (`sched/src/lib.rs:1574`)
  is the tail call that sets PC=`entry`, SP=`user_sp` and never returns (`syscall.rs:3006`).

### Address-space layout (verified)
| Region | Range | Source |
|---|---|---|
| ET_EXEC image + heap | from `0x20_0000` (2 MiB), heap grows up via brk | current userland; `elf` heap_start |
| `mmap` bump | grows **up** from `0x0000_4000_0000` (1 GiB) | `MMAP_BUMP` `syscall.rs:36` |
| single mmap cap | 256 MiB per call | `MAP_MAX_BYTES` `:1348` |
| aarch64 sigreturn trampoline | `0x0000_7fff_ff00_0000` | `sched/src/signal.rs:57` |
| user stack | `[0x7fff_ffff_c000, 0x7fff_ffff_f000)` | `:201`/`:203` |
| user VA ceiling | `0x0000_8000_0000_0000` | `USER_SPACE_END` `:198` |

### Syscall coverage (relevant to ld.so; verified present)
`openat`(`:955`), `pread64`(`:1134`→`sys_pread64 :2004`), `mmap`(`:878`→`:1371`, incl. MAP_FIXED
`:1392`/`:1411`/`:1474`/`:1545`, file-backed **eager** copy `:1544`+), `munmap`(`:879`),
`mprotect`(`:880`→`:912` in vmm), `brk`(`:881`), `mremap`(`:882`), `set_tid_address`(`:939`),
`arch_prctl`/ARCH_SET_FS (x86_64, `:944`), `getrandom`(`:1130`), `clock_gettime`(`:926`),
`madvise`→no-op(`:1132`). Static musl (S1) + pthreads already run, so **TLS setup is not a new gap**
(x86_64 via arch_prctl; aarch64 writes TPIDR_EL0 from EL0 directly, no syscall).

---

## 2. musl self-relocation — VERIFIED, no kernel relocator needed

### Static-PIE (ET_DYN, no PT_INTERP) — `crt/rcrt1.c` → `ldso/dlstart.c :_dlstart_c`
`rcrt1.c` aliases `_start_c` to `_dlstart_c`. The per-arch `_start` asm (`arch/x86_64/crt_arch.h`,
`arch/aarch64/crt_arch.h`) passes **`sp`** and **`&_DYNAMIC` computed PC-relative** (x86_64
`lea _DYNAMIC(%rip),%rsi`; aarch64 `adrp x1,_DYNAMIC; add x1,x1,:lo12:_DYNAMIC`). Because `&_DYNAMIC`
is PC-relative, it is correct **regardless of load bias**.

`_dlstart_c` then (non-FDPIC branch, `ldso/dlstart.c`):
```c
base = aux[AT_BASE];
if (!base) {                       // static-PIE: AT_BASE is 0
    Phdr *ph = (void *)aux[AT_PHDR];
    for (...; ph = ph+AT_PHENT)    // scan phnum=AT_PHNUM entries
        if (ph->p_type == PT_DYNAMIC) { base = (size_t)dynv - ph->p_vaddr; break; }
}
// then applies DT_REL/DT_RELA/DT_RELR RELATIVE relocs itself:
//   REL:  *rel_addr += base;
//   RELA: *rel_addr = base + rel[2];
//   RELR: *relr_addr += base;
```
**Conclusion (verified):** for a static-PIE the kernel maps at bias `B`, sets `AT_BASE = 0`,
and provides correct `AT_PHDR/AT_PHNUM/AT_PHENT`. musl computes `base == B` from
`&_DYNAMIC − PT_DYNAMIC.p_vaddr` and relocates itself. **Kernel applies no relocations.**
(Setting `AT_BASE = B` would also work — musl would use it directly — but `0` is the
Linux-faithful value and is what we spec.)

### Dynamic exe (PT_INTERP → ld-musl, itself ET_DYN) — `ldso/dynlink.c`
When the kernel jumps to the interpreter's entry, the interpreter's own `_dlstart_c` self-relocates
using **`base = aux[AT_BASE]`** (`dynlink.c :1721`: `if (aux[AT_BASE]) ldso.base = (void*)aux[AT_BASE]`).
So for PT_INTERP the kernel **must** set `AT_BASE = interpreter load base`. Then `__dls3`
(`:1802`+) processes the **main** program from the auxv:
- `:1818` `libc.page_size = aux[AT_PAGESZ]` → **AT_PAGESZ mandatory, nonzero.**
- `:1819` secure = `(aux[0]&0x7800)!=0x7800 || AT_UID!=AT_EUID || AT_GID!=AT_EGID || AT_SECURE`
  → provide AT_UID/EUID/GID/EGID (equal) **and AT_SECURE=0**, else env is stripped.
- `:1834`–`:1843` if `AT_PHDR != ldso.phdr`: `app.phdr=AT_PHDR; app.phnum=AT_PHNUM;
  app.phentsize=AT_PHENT; app.base = AT_PHDR − PT_PHDR.p_vaddr` → **AT_PHDR/PHNUM/PHENT of the main
  exe, biased, mandatory.**
- `:1856` AT_EXECFN → `app.name` (optional; skipped if it starts with "/proc/").
- `:1914`/`:2075` `CRTJMP((void*)aux[AT_ENTRY], argv-1)` → **AT_ENTRY = main exe biased entry, mandatory.**
- `:1781` `search_vec(auxv,&__hwcap,AT_HWCAP)` sets `__hwcap` **only if present**; absent ⇒ `__hwcap=0`.
  **AT_HWCAP=0 is safe on both arches** for bring-up (no pre-main code path gates on it fatally).
  AT_HWCAP2 likewise optional; omit or 0.
- `src/env/__libc_start_main.c:40` `__init_ssp((void*)aux[AT_RANDOM])` → AT_RANDOM already provided.

There is **no** kernel-side relocation requirement in either path. The fallback R_*_RELATIVE spec
(R_X86_64_RELATIVE=8, R_AARCH64_RELATIVE=1027) is **not needed** and is intentionally omitted.

---

## 3. auxv table to emit (final)

`bias` = 0 for ET_EXEC, `MAIN_DYN_BASE` for ET_DYN. `main.*` are read from the *main* binary's ehdr.

| Tag | # | Value | Required by | Notes |
|---|---|---|---|---|
| AT_PHDR | 3 | `bias + main.e_phoff` (== `bias + PT_PHDR.p_vaddr`) | ld.so + rcrt1 | already emitted; must add bias |
| AT_PHENT | 4 | `main.e_phentsize` | ld.so + rcrt1 | present |
| AT_PHNUM | 5 | `main.e_phnum` | ld.so + rcrt1 | present |
| AT_ENTRY | 9 | `bias + main.e_entry` | ld.so CRTJMP | **NEW, mandatory for PT_INTERP** |
| AT_BASE | 7 | PT_INTERP: `INTERP_BASE`; else `0` | ld.so self-reloc | **NEW** |
| AT_PAGESZ | 6 | `PAGE_SIZE` (4096) | ld.so (`page_size`) | present |
| AT_RANDOM | 25 | user VA of 16 rand bytes | SSP | present |
| AT_SECURE | 23 | `0` | env handling | **NEW** (else env stripped) |
| AT_UID/EUID/GID/EGID | 11/12/13/14 | current creds (0 today) | secure calc | present |
| AT_HWCAP | 16 | `0` | optional | **NEW**, 0 safe both arches |
| AT_EXECFN | 31 | user VA of argv[0] string | `app.name` (optional; falls back to argv[0] anyway) | **NEW** but optional |
| AT_LEANDROS_VFS/NET/AUDIO | 256/257/258 | ports | Leandros | keep |
| AT_NULL | 0 | 0 | terminator | keep |

**AT_CLKTCK deliberately NOT emitted:** verified in both musl 1.2.5 and 1.2.6 that
`sysconf(_SC_CLK_TCK)` is hardcoded to 100 (`src/conf/sysconf.c`) and AT_CLKTCK is never read — providing
it is a pure no-op. Pair count grows 13 → **~18** (AT_ENTRY, AT_BASE, AT_SECURE, AT_HWCAP mandatory
for the dynamic path; AT_EXECFN optional). Update the hardcoded `auxv_words = 13*2` (`syscall.rs:2896`)
and the array (`:2951`). AT_EXECFN can point at the already-written argv[0] string (`str_base_va + 0`),
avoiding a new string allocation; guard `argc==0 ⇒ AT_EXECFN=0` (musl then falls back to argv[0] itself).

---

## 4. Address-space layout for dynamic binaries

Fixed regions today leave a clean window `[image_top … 0x4000_0000)` (2 MiB … 1 GiB) below the
mmap bump, and the mmap bump itself is where ld.so's library mappings land (it mmaps with hint 0).

Chosen constants (justified against §1 table):
```
MAIN_DYN_BASE = 0x20_0000     (2 MiB)   -- bias for ET_DYN main (PIE)
INTERP_BASE   = 0x3000_0000   (768 MiB) -- bias for ld-musl-<arch>.so.1
```
- **`MAIN_DYN_BASE = 2 MiB`**: identical to today's ET_EXEC placement, so a PIE's first PT_LOAD
  lands at 2 MiB (off the null page), heap follows the image exactly as now. Zero divergence from
  the working static path; back-compat by construction.
- **`INTERP_BASE = 768 MiB`**: sits in the free window, 256 MiB below the `mmap` bump (1 GiB) so
  ld.so's own library mmaps (which grow up from 1 GiB) never reach it, and well above any realistic
  brk heap growing up from ~2 MiB. ld-musl's mapped span is ~1 MiB, so `[768 MiB, 768 MiB+~1 MiB)`.
- **Stack** unchanged (`0x7fff_ffff_c000+`); **AT_RANDOM/argv/envp** unchanged. aarch64 sigreturn
  trampoline (`0x7fff_ff00_0000`) is far from all of the above.

```
 0x0000_0000  +----------------+ null guard
 0x0020_0000  | MAIN image     |  <- MAIN_DYN_BASE (PIE) / ET_EXEC (today)
              | + brk heap ^   |
              |    (free window)|
 0x3000_0000  | ld-musl image  |  <- INTERP_BASE (~1 MiB span)
              |    (free window)|
 0x4000_0000  | mmap bump  ^^^ |  <- ld.so lib mappings, malloc arenas (grow up)
     ...      |                |
 0x7fff_ff00  | sigret tramp   |  (aarch64)
 0x7fff_ffc0  | user stack  vvv|
 0x8000_0000_0000  ceiling
```
Documented assumption: brk from the main image must not climb past `INTERP_BASE` (768 MiB of brk is
unrealistic for these programs; a brk that far already fails to map). If ever a concern, raise
`INTERP_BASE` — it is a single constant.

---

## 5. What ld-musl needs from the kernel at runtime (verified vs. coverage)

From `ldso/dynlink.c` `map_library()` (`:700`–`:860`), `load_library()` (`:1100`–`:1165`),
`reloc_all`/RELRO (`:1426`), `path_open`/`sys_path` (`:873`, `:1127`–`:1163`):

| ld.so action | syscall(s) | flags | kernel status |
|---|---|---|---|
| read phdrs of a lib | `pread64` | — | ✅ `sys_pread64 :2004` |
| reserve lib span | `mmap(0,map_len,MAP_PRIVATE,fd)` (`:809`) | file, **eager contiguous** | ⚠️ eager copies whole span, one contiguous phys block (~1 MiB, order ~8) — **fragmentation risk** |
| place each LOAD seg | `mmap(base+min,…,MAP_PRIVATE\|MAP_FIXED,fd)` (`:842`) | **FIXED overlay** | ⚠️ needs `unmap_range` **middle-split** (§6-A) |
| zero-fill bss | `mmap(…,MAP_PRIVATE\|MAP_FIXED\|MAP_ANONYMOUS,-1)` (`:848`) | FIXED anon | ✅ (same split dependency) |
| DT_TEXTREL fixup (rare) | `mprotect(map,map_len,RWX)` (`:854`) | **W+X** | ❌ kernel rejects W^X (`:1387`/`vmm:913`) — only PIC-with-textrel libs hit this; **not** normal .so |
| apply RELRO | `mprotect(relro_start,len,PROT_READ)` (`:1426`) | RO sub-range | ⚠️ needs `mprotect` **sub-range split** (§6-B) |
| TLS/malloc arenas | `mmap(0,n,RW,MAP_ANON\|MAP_PRIVATE)` (`dl_mmap :1018`) | anon | ✅ |
| find DT_NEEDED lib | `openat(name)` or search | O_RDONLY\|O_CLOEXEC | ✅ openat |
| search path (dlopen) | read `/etc/ld-musl-<arch>.path`, else `/lib:/usr/local/lib:/usr/lib` (`:1147`,`:1162`) | open+read | ✅ mechanically; **image must contain the file or the lib in /lib** |

**Key runtime facts (verified):**
- A **minimal dynamic exe whose only DT_NEEDED is libc.so opens *no* files at runtime**: musl
  resolves `libc.so` to the already-loaded ld.so itself (ld.so *is* libc). So `hello-dyn` needs only
  anon `mmap` + RELRO `mprotect`. Cleanest first bring-up target.
- ld.so does **not** reopen the main program in the PT_INTERP case (it uses AT_PHDR). `open(argv[0])`
  at `:1901` is only the "ldso invoked as a command" path, which we do not use.
- The interpreter path string (default `/lib/ld-musl-x86_64.so.1`, `/lib/ld-musl-aarch64.so.1`) must
  be **resolvable by the kernel via the normal VFS path lookup** (`open_kernel_path`) — i.e. the image
  must ship the interpreter at exactly that path. Packaging is a userland-wave item, but K3 must
  resolve it.

**Gaps to flag:** none block `hello-dyn`. `dlopen`/DT_NEEDED needs the two VMM splits (§6) and a
findable plugin (recommend the dlopen test use an **absolute** path first, deferring `/etc/ld-musl`).

---

## 6. The two load-bearing VMM changes (riskiest work)

`mm::vmm::AddressSpace` stores VMAs in a flat `regions` slot array and **cannot split one VMA into
two**. Both ld.so hot paths need splitting:

### 6-A. `unmap_range` middle-punch (vmm.rs:746)
Verified at `:810`–`:813`: a punch strictly inside a VMA does
`region.end = clip_s` ("back trim… middle → leave left part, **accept right leak** for eager").
The right portion is **lost from the VMA table** (and its eager phys leaked). ld.so's
`mmap(MAP_FIXED)` overlays punch the reservation VMA repeatedly; if the linker leaves any inter-segment
gap the second overlay is a middle-punch → the tail (including the region the *next* overlay/bss will
target) is no longer covered by a VMA. Modern lld/musl segments are often page-contiguous (each overlay
front-trims the shrinking reservation, which *does* work), so this can appear to work by luck and then
fail on a lib with a gap. **Fix: implement true middle-split** (allocate a second slot for
`[clip_e, r_end)`, copy `file_cap`/`phys`/`lazy_pages` tail, take a `file_release`/`pageref` as needed).

### 6-B. `mprotect` sub-range (vmm.rs:912)
Verified at `:933`–`:934`: `region.prot`/`region.flags` are set for the **entire** overlapping VMA,
while only the PTEs inside `[addr,end)` are remapped (`:942`,`:962`). RELRO's `mprotect(..,PROT_READ)`
targets a **sub-range** of the writable data VMA (default musl/lld put `.data.rel.ro`/`.got` inside the
RW LOAD segment). Result: the recorded `flags` for the *writable remainder* flip to read-only; a later
fault re-installs those pages read-only → silent corruption of `.data`. **Fix: split the VMA at
`[addr,end)` boundaries** (reuse the 6-A split primitive) so only the RELRO sub-VMA becomes RO.

Both reduce to one shared `split_at(boundary)` helper. This is the single highest-risk deliverable.

### 6-C. (secondary) eager file-backed reservation
`sys_mmap` file path is eager+contiguous (`:1544`). ld.so's reservation `mmap` of the whole lib span
allocates one contiguous phys block (~1 MiB order-8 for ld-musl; larger for bigger libs) — the exact
fragmentation the loader's `load_lazy` was created to avoid. Acceptable for the corpus; consider a
lazy/COW file-backed VMA later. Not a correctness blocker.

---

## 7. Back-compat decision tree (static relibc + S1 static musl unchanged)

```
parse_ehdr(main)
├─ e_type == ET_EXEC
│   ├─ no PT_INTERP  → bias=0; load()/load_lazy(bias=0); entry=e_entry;
│   │                  AT_BASE=0, no AT_ENTRY needed.   ← EXACT current path (relibc, S1 static)
│   └─ PT_INTERP     → bias=0; load main at 0; load interp @INTERP_BASE;
│                      entry=interp_entry; AT_BASE=INTERP_BASE; AT_ENTRY=e_entry.
└─ e_type == ET_DYN
    ├─ no PT_INTERP  → bias=MAIN_DYN_BASE; load(bias); entry=bias+e_entry;
    │                  AT_BASE=0; AT_ENTRY=bias+e_entry.   ← static-PIE (rcrt1 self-relocs)
    └─ PT_INTERP     → bias=MAIN_DYN_BASE; load main @bias; load interp @INTERP_BASE;
                       entry=interp_entry; AT_BASE=INTERP_BASE; AT_ENTRY=bias+e_entry. ← dynamic PIE
AT_PHDR/PHENT/PHNUM always describe the MAIN exe, biased.
```
The ET_EXEC/no-interp arm is byte-for-byte the code that runs today (bias 0 collapses every
`bias + x` back to `x`). Regression-safe by construction.

---

## 8. Touch list (file:function, both arches — code is arch-independent unless noted)

1. **`elf/src/lib.rs`**
   - add `const PT_INTERP: u32 = 3;`
   - `ElfInfo`: add `e_type: u16`, `interp: Option<(usize, usize)>` (file off,len of interp string), keep bias implicit in returned VAs.
   - `load(bytes, as_, bias)` / `load_lazy(header, as_, file_cap, bias)`: thread `bias` into every
     `vaddr` (`:198/:227/:349/:368`), `load_base` (`:207/:355`), `entry` (`:306/:398`), `highest`.
     Emit `interp` if a PT_INTERP is seen (from `header`/`bytes`). aarch64 icache path unchanged.
2. **`kernel/src/syscall.rs :: sys_execve` (:2660)**
   - after header parse: `bias = if ET_DYN {MAIN_DYN_BASE} else {0}`; pass to loader.
   - if `ElfInfo.interp`: ensure interp string is in buffer (pread if `p_offset+p_filesz` exceeds it),
     resolve path, load interpreter (`read_file_from_vfs`+`elf::load(.., INTERP_BASE)`), capture
     `interp_entry`, `AT_BASE=INTERP_BASE`; else `AT_BASE=0`, `entry=main.entry`.
   - expand auxv array + fix `auxv_words` count (`:2896`,`:2951`): add AT_ENTRY, AT_BASE, AT_SECURE=0,
     AT_HWCAP=0, AT_EXECFN(argv0 VA). (Do **not** add AT_CLKTCK — musl ignores it.)
   - `replace_address_space(.., entry=chosen_entry, ..)`.
   - add consts `MAIN_DYN_BASE`, `INTERP_BASE`.
3. **`kernel/src/syscall.rs :: open_exec_header` (:2578)** — optionally extend to cover the PT_INTERP
   string (or handle in execve via a pread). Small.
4. **`mm/src/vmm.rs`** — `split_at()` helper; wire into `unmap_range` (`:746`, replace `:810`–`:813`
   middle case) and `mprotect` (`:912`, split at `[addr,end)` before setting flags). Arch-independent.

Optional/deferred: lazy file-backed reservation (§6-C); `/etc/ld-musl-<arch>.path` provisioning
(userland wave); interpreter present in the image at `/lib/ld-musl-<arch>.so.1`.

---

## 9. Test plan (keyed to the parallel corpus)

Corpus dir `/Users/forain/.claude-forain/jobs/afde2e74/tmp/musl-dynamic/test/`:
`hello-dyn/` (C dynamic exe), `dlopen-host/` (host + `plugin.so`), `hello-dyn-rs/` (Rust dynamic-musl).
Verify each with `rust-objdump -p` (expect PT_INTERP `/lib/ld-musl-<arch>.so.1`, ET_DYN, `.rela.dyn`).

Progression (each on **both** x86_64 and aarch64 via `run-qemu.sh`):
1. **Static-PIE, no interp** — build/obtain an ET_DYN-without-PT_INTERP (musl static-PIE, e.g. default
   `rustc x86_64-unknown-linux-musl`). Exercises §2 static-PIE, `MAIN_DYN_BASE` bias, auxv AT_BASE=0.
   No VMM split needed. **First milestone — smallest surface.**
2. **`hello-dyn` (dynamic C, only libc.so)** — exercises PT_INTERP load, `INTERP_BASE`, AT_BASE/AT_ENTRY,
   ld.so self-reloc, RELRO `mprotect` (needs §6-B). **No file opens at runtime.**
3. **`hello-dyn-rs` (Rust dynamic-musl)** — same path, larger image; stresses eager reservation (§6-C).
4. **`dlopen-host` + `plugin.so`** — exercises the full `openat`+`pread64`+`mmap(MAP_FIXED)` overlay
   path (needs §6-A middle-split) and library search. Use an **absolute** dlopen path first; then add
   `/etc/ld-musl-<arch>.path` or `/lib` to test search.
5. **Regression (must be byte-identical behavior):** S1 static ET_EXEC musl binaries + the current
   relibc userland (`userland/{init,login,shell,...}`, `f2fstest`, `pthreadtest`, `sigtest`, `polltest`).
   These take the ET_EXEC/no-interp arm (bias 0) — assert boot still lands on the login prompt on both
   arches and the existing test baselines are unchanged.

Instrument `sys_execve` with a one-line serial trace of `(e_type, bias, interp?, entry, AT_BASE)` per
exec during bring-up; diff against expectations before trusting "it booted".

---

## 10. Risk ranking

1. **`unmap_range` middle-split absent** (§6-A) — MAP_FIXED overlays lose/leak VMA tail; can pass by
   luck on contiguous-segment libs then fail. Highest.
2. **`mprotect` whole-VMA flag clobber** (§6-B) — partial RELRO silently marks writable `.data`
   read-only on next fault. Silent, high.
3. **Interp string outside header buffer** — `open_exec_header` sizes only to the phdr table.
   Bounded fix.
4. **Eager contiguous file reservation** (§6-C) — fragmentation on larger libs. Medium.
5. **W+X `mprotect` rejected** — only DT_TEXTREL libs; not normal PIC .so. Low.
6. **auxv count desync** — hardcoded `13*2`; must bump in lockstep with the array. Low but easy to miss.

## 11. Estimated diff size
- `elf/src/lib.rs`: ~+45 / −15
- `kernel/src/syscall.rs`: ~+70 / −15 (interp load + auxv expansion + consts)
- `mm/src/vmm.rs`: ~+40 (split_at + two call sites)
- **Total ≈ 150–200 lines net.** Moderate. The loader/auxv half is straightforward; the VMM
  split (§6) is the concentration of risk and where review effort belongs.
