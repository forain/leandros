# Fault A — offline symbolization of the COSMIC-panel software-GL EL0 DATA ABORT (READ)

**Lane:** host-only, read-only. `/Users/forain/code/leandros` untouched (M7s wave owns tree/QEMU).
Pure offline disassembly of the shipped aarch64 GL stack with Apple-LLVM `objdump` (aarch64 target).

**Fault A signature (given):** deterministic EL0 DATA ABORT **READ**, **FAR small (0x1B or 0x10)** =
NULL-base + struct-field offset, faulting instruction at **constant page-offset 0xC3C** (PC & 0xFFF),
inside a dlopen'd GL lib mapped at 0x40000000+.

---

## TL;DR — tell M7s

> **Low-12 == 0xC3C does NOT uniquely name the instruction** — libgallium alone has 1660 instrs and
> **239 non-SP loads** at page-offset 0xC3C. To map it instantly, read the faulting lib's dlopen base
> from the crash (`/proc`-maps equivalent or the loader log), subtract from the faulting PC → full
> file-vaddr, then look it up in the table below. **The single strongest a-priori match for a READ at
> FAR=0x10 is `libgallium-25.3.6.so : partial_unroll+0x488` (file-vaddr `0xdf6c3c`, `ldr w8,[x0,#0x10]`).**
> But note the important negative result: **fault A is NOT a plain "unchecked create-return deref"** —
> both c3c loads that follow a `bl`-create in libgallium are already `cbz x0`-guarded. So the NULL was
> produced *upstream*; look at what fed the faulting object, not at the fault site's own call.
> **Predicted fix-class: Mesa-side null-guard (softpipe has no JIT → gallivm H1 mmap-collision does NOT
> apply here).** Kernel/mmap is only a secondary suspect — confirm with the pre-fault syscall trace.

---

## Coverage

Disassembled every shipped aarch64 GL lib (cross-checked against `scripts/mkfs-f2fs-populated.py`
ship list: libEGL.so.1, libGLESv2.so.2, libgbm.so.1, libdrm.so.2, libgallium-25.3.6.so, libexpat.so.1,
libz.so.1, libwayland-egl.so.1, libffi.so.8, gbm/dri_gbm.so). libgallium is **softpipe-only** (82 MB,
full local symtab retained — 20,737 function symbols — so every candidate symbolizes exactly).

Filter: instructions whose file-vaddr `& 0xFFF == 0xC3C`, restricted to loads (`ldr/ldrb/ldrh/ldur/ldp/
ldrsw/…`). libgallium: 1660 c3c instrs → 314 loads → **239 loads off a non-SP base**. Small libs: a
handful each, none fitting (see table). SP-relative loads excluded (SP is never NULL).

---

## The two hard facts that reshape the hypothesis

1. **There is NO immediate `#0x1b` LOAD at page-offset 0xC3C in ANY shipped lib.** The only `…, #0x1b`
   at c3c in libgallium is a **`strb` (WRITE)** at `0xa2bc3c` — excluded (fault A is a READ). ⇒ A
   FAR=0x1B **read** can only come from a *register-offset* load `[Xn, Xm]` with Xn=NULL and index
   Xm=0x1B. Every such site at c3c (the `translate_*` index-convert family) dereferences its base at an
   **earlier** index first, so a NULL base would fault before reaching c3c, and an in-bounds base with a
   bad index gives a **large** FAR, not 0x1B. ⇒ **0x1B and 0x10 are almost certainly NOT the same
   instruction** — likely two fields of one NULL object hit at two PCs (M7s pinned 0xC3C for the 0x10
   one), or an approximate FAR read. Treat FAR=**0x10** as the load-bearing datum.

2. **No unchecked create-return deref exists at 0xC3C.** Exactly two c3c loads follow a `bl`-create:
   - `0xadec3c` `_mesa_GetMultiTexLevelParameterfvEXT`: `bl _mesa_get_texobj_by_target_and_texunit;`
     **`cbz x0, …`** ; `ldrh w24,[x0,#0x8]` — guarded.
   - `0xb25c3c` `st_TexSubImage`: `bl util_format_description;` **`cbz x0, …`** ; `ldr w8,[x0,#0x24]` — guarded.
   Both null-check. So the "`bl create; ldr [x0,#0x10]` no-cbz" pattern the brief hypothesized is
   **absent** from the shipped softpipe stack at this page-offset.

---

## Ranked candidates (READ, FAR-compatible, at page-offset 0xC3C)

Only the 7 immediate `[Xn,#0x10]` loads can give FAR=0x10 directly. Ranked by NULL-base plausibility +
software-GL-render reachability:

| # | file-vaddr | lib / symbol | instruction | base origin | verdict |
|---|-----------|--------------|-------------|-------------|---------|
| **1** | `0xdf6c3c` | libgallium **`partial_unroll+0x488`** | `ldr w8,[x0,#0x10]` | x0 = pass input reloaded from `[sp]` (saved at entry, `df6880`); reached via `cbz w8,→df6c3c` branch. Reads a `nir` node type field (`+0x10`), then `cmp w8,#0x2`. | **BEST fit.** NIR loop-unroll runs during first-draw shader compile. FAR=0x10 exact. NULL ⇒ upstream nir alloc/clone returned NULL. Caveat: only reached if a panel shader contains a loop. |
| 2 | `0xf42c3c` | libgallium `write_depth_stencil_values+0x404` | `ldr w11,[x0,#0x10]` | x0 = persistent per-quad ptr; **dereffed at `+0xc` earlier (`f42bfc`)** and `+0x27` (`f42c18`). | LOW — a NULL x0 faults at `+0xc` first (page-offset …bfc), never reaching c3c. Softpipe raster loop; x0 is valid here. |
| 3 | `0xa28c3c` | libgallium `_mesa_marshal_DrawArraysInstanced_no_error` | `ldr w9,[x8,#0x10]` | x8 = glthread batch ptr; **`ldp …,[x8,#0x18]` at `a28c38` derefs first**. | Out — earlier +0x18 deref faults first; glthread, not softpipe render. |
| 4 | `0xbebc3c` | libgallium `builtin_builder::create_intrinsics+0x3548` | `ldr x20,[x19,#0x10]` | x19 = stable `this` (GLSL builtin builder). | Out — `this` is non-NULL; GLSL compiler init, not per-draw NULL. |
| 5 | `0xdcfc3c` / `0xddac3c` | libgallium `evaluate_fall_equal8` / `evaluate_imsubshl_agx` | `ldr s0,[x10,#0x10]` / `ldp …,[x3,#0x10]` | NIR const-fold src operands. | Out — const-fold operands are valid; `_agx` variant is dead on softpipe. |
| 6 | `0xecac3c` | libgallium `atomic_decl_range_sort+…` | `ldr w8,[x8,#0x10]` | sort iterator. | Out — compile-time gather/sort; iterator non-NULL. |

**Register-offset family (only path that could yield FAR=0x1B), all ruled out:** `translate_*` index
converters (`0xe64c3c`, `0xe67c3c`, `0xe70c3c`, `0xe82c3c`, …) do `ldrb/ldrh [x0, w<idx>, uxtw]` with x0
= source index buffer. Each c3c load is the **2nd/3rd** index read; the 1st (at `…c2c`/`…c1c`) faults
first if x0=NULL, and a valid x0 with a bad index gives a large FAR. Also `softpipe_set_stencil_ref+0x8`
/`softpipe_bind_depth_stencil_state` `ldr w9,[x0,x8]`: x8 is the **constant** 0x7480 and a `strh
w1,[x0,#0xb70]` **WRITE** precedes — a NULL x0 would be a WRITE abort at FAR=0xb70, not a small READ.

**Small libs** (for completeness — none fit a small-offset NULL READ at c3c):
`libdrm drmSLDestroy+0x4c ldr x22,[x0,#0x20]` (0x20); `libEGL eglInitialize+0x7fc ldr q0,[x9]`,
`parseOneConfigFile ldr w9,[x9,#0x8]`; `libGLESv2 glBindVertexArray+0x20 ldr x9,[x9,x0]` (dispatch
lookup), `glGetSamplerParameterIiv+0x30 ldr x3,[x10,#0x1580]` (TLS); `libGLESv1_CM glViewport+0x28
ldr x10,[x10,x0]`. dri_gbm.so / libgbm: no qualifying loads at c3c.

---

## Top candidate — NULL source & fix-class

**`libgallium-25.3.6.so : partial_unroll+0x488` (`0xdf6c3c`, `ldr w8,[x0,#0x10]`).**

- x0 is `partial_unroll`'s input node (a `nir_loop`/`nir_cf_node`), saved to `[sp]` at entry and
  reloaded at the fault. A NULL here means an **upstream NIR allocation returned NULL** (nir uses
  `ralloc`, i.e. plain malloc — **not** JIT/mmap-hinted memory).
- **This is why gallivm-null-analysis.md's H1 (MMAP_BUMP non-FIXED-hint collision) does NOT directly
  apply:** H1 requires LLVM `SectionMemoryManager` *hinted* mmaps to poison the bump. **Softpipe ships
  with `-Dllvm=disabled` — there is no JIT and no hinted mmap**, so the bump stays consistent and a
  plain `calloc→mmap(addr=0)` just bumps normally and succeeds. A softpipe/NIR NULL is therefore *not*
  the H1 signature.
- ⇒ **Predicted fix-class = Mesa-side null-guard** (a genuine unchecked-NULL, latent everywhere, that
  LeandrOS reaches because some object create fails for a capability/syscall reason — cf.
  mesa-caps-matrix.md: softpipe on the deviceless path has **NO `EGL_EXT_image_dma_buf_import`**, so a
  dmabuf-import / EGLimage path returns NULL that the caller may deref). **Kernel/mmap is a secondary
  suspect only** — accept it only if the pre-fault syscall trace shows an anonymous `mmap` returning
  `-ENOMEM` or a driver ioctl failing immediately before the abort.

---

## Lookup table for M7s (map any 0xC3C fault instantly)

Once you have `(lib_base, faulting_PC)`: `file_vaddr = PC − lib_base`; confirm `file_vaddr & 0xFFF ==
0xC3C`; match `file_vaddr` against the render-relevant rows above (full symbolized list of all 239
libgallium c3c loads is in `~/code/leandros-artifacts/tmp-faultA/joined.txt`). If the lib is **not**
libgallium, use the small-lib rows. The dlopen base on LeandrOS is the `MMAP_BUMP`-placed 0x40000000+
address of the `.text` segment — subtract the segment's file p_vaddr if the loader logs the mapping.

---

## Checkpoint

- 2026-07-25: Offline-symbolized fault A against the full shipped aarch64 softpipe GL stack. **No
  unchecked create-return deref exists at page-offset 0xC3C** (the 2 bl-preceded c3c loads are
  `cbz x0`-guarded); **no immediate `#0x1b` READ exists at c3c anywhere** (0x1B must be register-offset,
  and every such site faults earlier). Strongest single match for a READ@FAR=0x10 = **libgallium
  `partial_unroll+0x488` (`0xdf6c3c`)**. Predicted fix-class = **Mesa null-guard**, not kernel mmap:
  softpipe has no JIT, so gallivm-null H1 does not apply. Action for M7s: capture `(lib_base, PC)` to
  disambiguate the 239 c3c load-sites, and trace what produced the NULL object upstream (the fault site
  itself does not create it). Artifacts: `tmp-faultA/gallium_c3c.txt`, `tmp-faultA/joined.txt`,
  `tmp-faultA/funcsyms.txt`.
