# M7t "null __DRI* extension" — ROOT-CAUSED to the kernel `mincore` stub (not a DRI driver)

**Lane:** host-only, read-only. `/Users/forain/code/leandros` untouched. Offline disassembly of
the shipped aarch64 GL stack (Apple-LLVM objdump/readelf) + Mesa 25.3.6 source
(`~/code/leandros-artifacts/llvmpipe-lane/src/mesa`) + the M7t on-target VMA/backtrace capture
(`notes/m7t-logs/m7t-cap6-serial.log`).

---

## TL;DR — for the tree wave

> **There is NO null `__DRIextension`, and NO client DRI driver is missing from the ship set.** The
> premise ("28KB DRI/swrast lib deref'ing a null `__DRI*`") is a **misidentification**. The faulting
> 28KB lib is **`libwayland-client.so.0`** (its exec segment maps to the same 0x7000 page-span as
> `dri_gbm.so`, which is why M7t guessed "DRI/swrast"). The crash is:
>
> ```
> wl_proxy_create_wrapper((struct wl_surface *)3)   →   ldr x8, [x19, #0x18]   →   FAR = 3 + 0x18 = 0x1B
> ```
>
> where **`3` = `WL_EGL_WINDOW_VERSION`**. Mesa's `get_wayland_surface()` misreads the
> `wl_egl_window`'s integer `version` field as a `wl_surface*` because
> **`_eglPointerIsDereferenceable((void*)3)` wrongly returns TRUE on LeandrOS.** It returns TRUE
> because the kernel's `mincore` syscall is a stub that always returns success:
>
> ```
> kernel/src/syscall.rs:1026:   MINCORE  => 0, // pretend all pages are resident
> ```
>
> **FIX (cheapest, correct, one handler): make `mincore` POSIX-correct** — return `-ENOMEM` (-12)
> when the queried range contains an unmapped page. That is the exact signal Mesa's probe relies on.
> **Try this first.** No Mesa rebuild, no staging change, no env change, no render-path patch.

---

## The fault (on-target, deterministic 9/9 — `m7t-cap6-serial.log`)

```
[EXC] EL0 Fault! PID=37 ESR=92000006 FAR=000000000000001B EC=0x24 DFSC=6(translation) WnR=0(READ) ELR=8C793C3C
[BT] elr=8C793C3C lr=8C793C34
[BT] 0 ret=8F7BF3F8   ← libEGL + 0x1C3F8   (loader_wayland_wrap_surface, right after bl wl_proxy_create_wrapper)
[BT] 1 ret=8F7B8970   ← libEGL + 0x15970   (dri2_wl_create_window_surface + 0x3c8, right after bl get_wayland_surface)
[BT] 2 ret=8F7B30F4   ← libEGL
[BT] 3 ret=8F7A5464   ← libEGL            (eglCreatePlatformWindowSurfaceEXT entry region)
[BT] 4 ret=00717704   ← cosmic-panel (ClientEglSurface::create)
```

VMA map (executable regions only) from the same crash — the faulting `[VMA]*` is 0x7000 = 28 KB:

```
[VMA]* start=8C792000 end=8C799000 prot=R+X   ← FAULT lib  (0x7000 span)
[VMA]  start=8F7A3000 end=8F7C4000 prot=R+X   ← libEGL     (0x21000, matches exec memsz 0x20af8)
[VMA]  start=900EF000 end=9076C000 prot=R+X   ← libgallium (0x67D000, softpipe — NOT the fault lib)
```

---

## Step 1 — the "28KB DRI/swrast lib" is `libwayland-client.so.0`

The LeandrOS ELF loader maps each exec `PT_LOAD` at `load_base + page_floor(p_vaddr_exec)` and spans
`ceil((p_vaddr&0xfff + memsz)/4K)` pages. Calibrated against known anchors (libEGL exec 0x20af8→0x21000,
libffi 0x2e9c0→0x2f000, libgallium→0x67D000 — all exact). Under that rule **exactly two** shipped libs
map to a 0x7000 (28 KB) span:

| lib | exec p_vaddr | memsz | page span |
|-----|-------------|-------|-----------|
| `libwayland-client.so.0` | 0x17a78 | 0x5c08 | 0x17000→0x1e000 = **0x7000** |
| `gbm/dri_gbm.so`         | 0x14724 | 0x5a1c | 0x14000→0x1b000 = **0x7000** |

Disassembling both at the fault offset (region + 0x1C3C) is decisive:

```
# libwayland-client.so.0  @ vaddr 0x18C3C:
0000000000018c18 <wl_proxy_create_wrapper>:
   18c30: bl   calloc@plt
   18c38: cbz  x0, ...                 ; calloc result IS null-checked
   18c3c: ldr  x8, [x19, #0x18]        ; <-- FAULT (a READ). x19 = arg0 = the proxy to wrap.

# gbm/dri_gbm.so  @ vaddr 0x15C3C:
   15c3c: mov  w3, #0x1                ; NOT a load — cannot be a data-abort. RULED OUT.
```

So the fault lib is **`libwayland-client.so.0`**, faulting instruction **`wl_proxy_create_wrapper+0x24:
ldr x8,[x19,#0x18]`** (reads `proxy->display`). `dri_gbm.so` is excluded. `libgallium` (softpipe, the
actual swrast driver) is at 0x900EF000 — **not** involved in the fault, confirming M7s's "no JIT /
not llvmpipe."

**Note for `faultA-symbolize.md`:** that host-lane analysis searched libgallium + a short small-lib
list for the 0xC3C page-offset load but **did not include `libwayland-client.so.0`** in the set, which
is why it landed on `partial_unroll` in libgallium. The M7t VMA capture (fault base 0x8C792000 ≠
libgallium 0x900EF000) supersedes it: the fault is not in libgallium at all.

---

## Step 2 — the caller: `get_wayland_surface` passing a bogus `wl_surface*`

libEGL frame [BT]1 is inside `dri2_wl_create_window_surface`, immediately after `bl get_wayland_surface`
(disasm: `4296c: bl get_wayland_surface ; 42970: tbz w0,#0, ...`). [BT]0 is inside
`loader_wayland_wrap_surface`, which is what `get_wayland_surface` tail-calls
(`platform_wayland.c:479`). Source (`platform_wayland.c:465-482`):

```c
static bool
get_wayland_surface(struct dri2_egl_surface *dri2_surf, struct wl_egl_window *window)
{
   struct wl_surface *base_surface;
   /* Version 3 of wl_egl_window introduced a version field at the same location
    * where a pointer to wl_surface was stored. Thus, if window->version is
    * dereferenceable, we've been given an OLDER wl_egl_window and version points
    * to wl_surface. */
   if (_eglPointerIsDereferenceable((void *)(window->version)))
      base_surface = (struct wl_surface *)window->version;   // ← WRONGLY TAKEN
   else
      base_surface = window->surface;                        // correct branch
   return loader_wayland_wrap_surface(&dri2_surf->wayland_surface, base_surface, dri2_surf->wl_queue);
}
```

`wl_egl_window.version == WL_EGL_WINDOW_VERSION == 3` for any window built by `wl_egl_window_create`
(what cosmic-panel's `WlEglSurface::new` calls). If the deref-probe returns TRUE for `(void*)3`, Mesa
sets `base_surface = (wl_surface*)3` and calls `wl_proxy_create_wrapper((wl_surface*)3)` →
`ldr x8,[(void*)3 + 0x18]` → **FAR = 0x1B**. The reported FAR = 0x1B is an *exact* arithmetic match
(3 + 0x18); this is the load-bearing datum. (The secondary "0x10" the earlier waves quoted was a
looser pre-capture reading; the deterministic captured value is 0x1B and it pins the mechanism exactly.)

---

## Step 3 — why the probe lies: the kernel `mincore` stub

`_eglPointerIsDereferenceable` (`src/egl/main/eglglobals.c:125`), built **with** `HAVE_MINCORE`
(both arch build logs: `Checking for function "mincore" : YES`):

```c
EGLBoolean _eglPointerIsDereferenceable(void *p) {
   uintptr_t addr = (uintptr_t)p;                 // p = 3
   ...
   if (p == NULL) return EGL_FALSE;
   addr &= ~(page_size - 1);                       // addr → 0
   if (mincore((void *)addr, page_size, &valid) < 0)
      return EGL_FALSE;                            // Linux: page 0 unmapped → -1 → FALSE (correct)
   return EGL_TRUE;                                // LeandrOS: mincore returns 0 → TRUE (WRONG)
}
```

The probe deliberately ignores the residency vector; it treats **"`mincore` did not fail"** as
"the page is mapped, so `p` is a real pointer." LeandrOS breaks that contract:

```
kernel/src/syscall.rs:1026:   MINCORE  => 0, // pretend all pages are resident
```

`mincore` is a bare `=> 0` arm: it ignores `addr/length/vec` and always returns success — even for the
unmapped page 0. Hence `_eglPointerIsDereferenceable((void*)3) == EGL_TRUE`, the wrong branch is taken,
and the panel dies at `eglCreatePlatformWindowSurfaceEXT`. (Sanity check on the `#else` path: had Mesa
been built *without* mincore, the fallback `return addr >= page_size` gives `3 >= 4096 == FALSE` — the
correct answer. So the bug requires BOTH the build-time mincore detection AND the kernel's lying stub.)

This is a general hazard, not Mesa-specific: any `mincore`-based "is this a valid pointer" heuristic
(and any residency query) is silently wrong on LeandrOS today.

---

## VERDICT + ranked fix (by cost)

**Is a client swrast DRI driver missing from the ship set?  → NO.** Mesa 25.3.6 has no separate
`swrast_dri.so` for the EGL path: `dri_target.c` builds the swrast/kms_swrast drivers *into*
`libgallium-25.3.6.so` (the megadriver), and EGL reaches them via a direct `driCreateNewScreen3()`
call — no `dri/*.so` dlopen (same mechanism the working GBM path — kmscube/anvil — uses). libgallium is
staged and is *not* the fault lib. Staging is not the problem.

| # | fix | class | cost | verdict |
|---|-----|-------|------|---------|
| **1** | **Make `mincore` POSIX-correct**: return `-ENOMEM` when the range `[a0, a0+a1)` contains an unmapped page (route through the VMM region lookup already used by demand-paging). At minimum, fail for an unmapped first page. `kernel/src/syscall.rs:1026`. | **kernel** | **~1 handler** | ✅ **DO THIS FIRST.** Directly restores the exact signal Mesa's probe needs; correct for all future `mincore` users; zero userspace churn. |
| 2 | Rebuild Mesa with mincore disabled so `_eglPointerIsDereferenceable` uses the `addr >= page_size` fallback (patch `src/egl/meson.build:165` `cc.has_function('mincore')` → false, or `#undef HAVE_MINCORE`). | Mesa rebuild | full GL rebuild + reship | ⚠️ Works, but far more expensive than the kernel one-liner and leaves the lying `mincore` latent for other callers. Fallback only. |
| 3 | Missing `swrast_dri.so` / staging fix | — | — | ❌ **REFUTED** — no such file exists in Mesa 25.x; swrast is in the staged libgallium. |
| 4 | Client env (`LIBGL_ALWAYS_SOFTWARE` / `GALLIUM_DRIVER` / EGL platform) | — | — | ❌ No env variable affects `mincore` or the `wl_egl_window` version probe. |
| 5 | Patch cosmic-panel to render its bar via wl_shm/pixman instead of client GlesRenderer | client render-path | large | ❌ Unnecessary — the client GL path is fine once `mincore` behaves. |

### After the fix
With `mincore` returning `-ENOMEM` for page 0, `_eglPointerIsDereferenceable((void*)3)` → FALSE →
`base_surface = window->surface` (the real layer-shell surface cosmic-panel passed to
`wl_egl_window_create`) → `wl_proxy_create_wrapper` gets a valid proxy →
`eglCreatePlatformWindowSurfaceEXT` completes. The panel then renders through the softpipe swrast +
`wl_shm` client path (`dri2_initialize_wayland_swrast`), which is the standard Mesa software path and
independent of this bug. Fault A is expected to clear entirely; watch for any *downstream* softpipe
render issues separately (not blocked on a null extension).

---

## Checkpoint

- 2026-07-25: Root-caused the M7t "null `__DRI*` extension" panel crash. It is **not** a `__DRIextension`
  and **not** a missing DRI driver. The 28KB fault lib = `libwayland-client.so.0` (misidentified as
  "DRI/swrast"; same 0x7000 exec span as `dri_gbm.so`, disambiguated by disasm: the fault instr is
  `wl_proxy_create_wrapper+0x24 ldr x8,[x19,#0x18]`, a READ; dri_gbm at that offset is a `mov`, ruled
  out). Chain: `get_wayland_surface` (platform_wayland.c:474) misreads `wl_egl_window.version==3`
  (`WL_EGL_WINDOW_VERSION`) as a `wl_surface*` because `_eglPointerIsDereferenceable((void*)3)` returns
  TRUE, because the kernel `mincore` stub (`kernel/src/syscall.rs:1026` `MINCORE => 0`) always reports
  success. FAR = 3 + 0x18 = **0x1B** (exact). **Fix = make `mincore` return `-ENOMEM` for unmapped
  pages (kernel one-liner). No Mesa rebuild / staging / env / render-path change needed.** Cross-refs:
  `notes/faultA-symbolize.md` (superseded — its search set omitted libwayland-client),
  `notes/mesa-caps-matrix.md`, `m3-gl-stack/NOTES.md`.
