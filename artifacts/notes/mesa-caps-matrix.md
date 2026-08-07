# Mesa capability matrix — softpipe (current on-target) vs llvmpipe (Alpine lane)

Host-only analysis lane. Repo `/Users/forain/code/leandros` READ-ONLY (read for evidence only).
Mesa source examined: `~/code/leandros-artifacts/llvmpipe-lane/src/mesa` (VERSION 25.3.6 — the exact
source both ship sets were built from). Empirical probes: Alpine 3.21 containers, both arches.

- **Current on-target ship set** = `m3-gl-stack/sysroot-<arch>/usr/lib/` — libgallium = **softpipe ONLY**
  (82 MB, zig-built, `-Dllvm=disabled -Dgallium-drivers=softpipe`; verified: zero `llvmpipe` strings).
- **Candidate ship set** = `llvmpipe-lane/stage-<arch>/usr/lib/` — libgallium = **llvmpipe+softpipe**
  (22 MB, Alpine-built, `-Dllvm=enabled -Dgallium-drivers=llvmpipe,softpipe`, shared libLLVM 19.1.4).
- Both: identical sonames (libEGL.so.1 / libGLESv2.so.2 / libGLESv1_CM.so.1 / libgbm.so.1 /
  libgallium-25.3.6.so / gbm/dri_gbm.so) → true drop-in. Both include `gbm/dri_gbm.so` +
  the kms_swrast/kms-dri sw winsys → **scanout parity: no gap-closure rebuild required** (Task 2 = N/A).

---

## 1. THE CAPABILITY MATRIX

There are **two distinct EGL paths** and they behave differently — conflating them is the single
biggest source of confusion in the M5 investigation, so the matrix is split by path.

### Path A — deviceless / surfaceless EGL (no DRM fd)
This is the path cosmic-comp lands on when it uses the **software renderer fallback**
(`EGL_MESA_device_software`), i.e. when `determine_primary_gpu()` filters out `is_software` devices.
**Empirically measured** (caps_probe.c, `EGL_PLATFORM_SURFACELESS_MESA`), BOTH arches identical:

| Extension                               | softpipe | llvmpipe |
|-----------------------------------------|:--------:|:--------:|
| EGL_KHR_image_base                      |   YES    |   YES    |
| EGL_EXT_image_dma_buf_import            |  **NO**  |  **YES** |
| EGL_EXT_image_dma_buf_import_modifiers  |  **NO**  |  **YES** |
| EGL_MESA_image_dma_buf_export           |    NO    |    NO    |
| GL_RENDERER                             | softpipe | llvmpipe (LLVM 19.1.4, 128/256 bits) |

Logs: `llvmpipe-lane/logs/caps-softpipe-aarch64.log`, `caps-llvmpipe-aarch64.log`,
`caps-x86_64-both.log`. rc=0 all four.

### Path B — GBM over /dev/dri/card0 (the on-target scanout path)
Container has no /dev/dri, so this row is **source-derived + corroborated by the tree wave's own
on-target evidence** (anvil run, `notes/m5-progress.md:325`). This is what cosmic-comp uses once
forced onto the GBM renderer (`COSMIC_RENDER_DEVICE=226:0`).

| Capability                              | softpipe | llvmpipe |
|-----------------------------------------|:--------:|:--------:|
| EGL_KHR_image_base                      |   YES    |   YES    |
| EGL_EXT_image_dma_buf_import            |  YES\*   |   YES    |
| EGL_EXT_image_dma_buf_import_modifiers  |  YES\*   |   YES    |
| EGL_MESA_image_dma_buf_export           |  YES\*   |   YES    |
| `gbm_bo_create()` (implicit modifier)   |   YES    |   YES    |
| `gbm_bo_create_with_modifiers(LINEAR)`  |  **NO**  |  **YES** |
| Usable dmabuf modifiers advertised      | **none** | LINEAR   |

\* **softpipe import/export on the GBM path exists ONLY because the LeandrOS kernel deliberately
lies about DRM_CAP_PRIME.** See mechanism below. Empirically confirmed by the tree wave: anvil on
the current softpipe build logs `EGL_EXT_image_dma_buf_import + _modifiers + EGL_MESA_image_dma_buf_export,
has_import_dmabuf:true has_export_dmabuf:true` on the `PLATFORM_GBM_KHR` display (`m5-progress.md:325`).

### Why they differ — the exact source mechanism
- EGL advertises import iff `pscreen->caps.dmabuf & DRM_PRIME_CAP_IMPORT`; export iff EXPORT bit;
  import+modifiers ride together on the import bit; `KHR_image_base` is unconditional
  (`src/egl/drivers/dri2/egl_dri2.c:597-599, 685-695`). Whole block is `#ifdef HAVE_LIBDRM`
  (both builds link libdrm → compiled in).
- **softpipe** never sets `caps.dmabuf` itself. It only wires `get_screen_fd`
  (`sp_screen.c:453`), so `caps.dmabuf` is whatever `u_screen.c:135-142` derives from
  `drmGetCap(fd, DRM_CAP_PRIME)`. **Purely kernel-dependent.** Deviceless (no winsys fd) →
  `get_screen_fd` = -1 → drmGetCap never runs → `caps.dmabuf = 0` → no import (matches Path A).
- **llvmpipe** sets `caps.dmabuf = DRM_PRIME_CAP_IMPORT|EXPORT` **unconditionally** whenever
  `winsys->get_fd` exists (the kms-dri/GBM case), *without* asking the kernel
  (`lp_screen.c:198-200`). Deviceless, it falls to the udmabuf branch → `DRM_PRIME_CAP_IMPORT`
  even with no /dev/udmabuf (`lp_screen.c:201-206`; build compiled `linux/udmabuf.h : YES`,
  `logs/build-*.log:229`) → import advertised (Path A). llvmpipe is strictly ≥ softpipe on every cell.
- **Modifiers:** `llvmpipe` implements the full modifier interface — `query_dmabuf_modifiers`,
  `is_dmabuf_modifier_supported`, `resource_create_with_modifiers`, all returning/accepting
  `DRM_FORMAT_MOD_LINEAR` (`lp_texture.c:1760-1826`). **softpipe implements none of them** (the
  callbacks are simply absent). So `gbm_bo_create_with_modifiers` → `dri2.c:882` sees a NULL
  `is_dmabuf_modifier_supported` → returns NULL on softpipe, succeeds (LINEAR) on llvmpipe. This
  is a **winsys-independent driver capability**, and it is the ONE real Mesa-side gap between the
  two builds on the GBM path. (Confirmed empirically by the tree wave: `m5-progress.md:261`
  "gbm_bo_create_with_modifiers(mod_linear/mod_invalid) returns NULL".)

### Kernel corroboration (read-only)
`drivers/src/drm_device_interface.rs:1114-1132` (committed a7754d0) — the standard
`DRM_IOCTL_GET_CAP` handler returns `DRM_CAP_PRIME => IMPORT|EXPORT`, with a comment stating it does
so *specifically* because "Mesa's softpipe (our only sw rasterizer) gates its dmabuf path on
drmGetCap(DRM_CAP_PRIME)". It returns `DRM_CAP_ADDFB2_MODIFIERS => 0`. This is the workaround that
makes softpipe advertise import/export on Path B despite softpipe never querying caps itself — and
it exactly matches the source trace above.

---

## 2. GAP CLOSURE

**No rebuild needed.** The llvmpipe stage is already a **strict superset** of the softpipe ship set:
same six sonames + `gbm/dri_gbm.so` + kms_swrast winsys (both arches — verified
`stage-<arch>/usr/lib/gbm/dri_gbm.so` present, 135 KB aarch64 / 82 KB x86_64), plus it adds the
`llvmpipe` gallium driver and the LINEAR-modifier interface softpipe lacks. DT_NEEDED closure and the
6 new deps (libLLVM.so.19.1 + libstdc++/libgcc_s/libzstd/libxml2/liblzma) are already staged and
verified in `deps-<arch>/` (see `llvmpipe-lane/NOTES.md`).

**No decisive negative.** Software dmabuf import + LINEAR modifiers ARE available for llvmpipe in
25.3.6 (empirically shown). The only thing genuinely unavailable for *any* software rasterizer here
is **non-LINEAR / tiled modifiers** (llvmpipe advertises LINEAR only; the KMS side advertises none,
`DRM_CAP_ADDFB2_MODIFIERS=0`) — which is correct and sufficient for a CPU compositor.

---

## 3. WHAT THIS MEANS FOR THE M5 BLOCKER (read this before deciding)

The task brief's premise — "current Mesa lacks EGL_EXT_image_dma_buf_import and modifier support, so
smithay can't build a reusing swapchain" — was the **early** diagnosis. The tree wave's later
evidence (`notes/m5-progress.md`, M5c section) supersedes it, and my source/empirical work reconciles
with the *later* view:

1. **The EGL extension gap is NOT the on-target blocker.** On the GBM/card0 path, the *current
   softpipe* build already advertises import+modifiers+export (kernel PRIME workaround); anvil proves
   it renders and presents. The "Dmabuf import extension not available" WARN comes only from
   cosmic-comp's **software-renderer fallback** (deviceless EGL, Path A), which it enters because its
   `is_software` GPU filter (`cosmic-comp .../kms/mod.rs:272`) rejects the software card. The
   intended fix is the userspace env `COSMIC_RENDER_DEVICE=226:0`, not a Mesa change.
2. **The remaining hard blocker is kernel/VFS**, owned by the M5d wave: cosmic-comp (smithay efeb597)
   reallocates + PRIME-exports its scanout buffer every frame, exhausting the shared 128-slot
   tmpfs dmabuf pool (`MAX_TMP_FILES`) and leaking fds on the INSTALL_FAIL path. Mesa cannot fix this.
3. **The one thing llvmpipe genuinely adds on Path B is LINEAR `gbm_bo_create_with_modifiers`.**
   Whether that eliminates the per-frame reallocation is **plausible but UNVERIFIED**: smithay's
   `GbmAllocator` (efeb597 `gbm.rs:204-232`, the `not(create_with_modifiers2)` branch LeandrOS's
   failed cc-probe selects) *falls back* to plain `gbm_bo_create` when `_with_modifiers` fails and the
   list contains Invalid/Linear — so softpipe still allocates buffers (as `implicit=true`). The churn
   the tree wave observes happens even on the GBM path, and they attribute it to smithay's
   per-frame design + the kernel pool leak, **not** to the modifier gap. llvmpipe *might* let
   smithay build an explicit-LINEAR reusing swapchain and cut the export rate — but that is a
   hypothesis to test, not a proven fix.
4. **On aarch64 (where M5 is being tested), llvmpipe cannot even run** until the kernel sets
   `SCTLR_EL1.UCI`(bit26)+`UCT`(bit15) — otherwise the first JIT `__clear_cache` traps (EC 0x18) and
   kills the process (`notes/kernel-readiness-audit.md` Task 1). So "swap Mesa to avoid a kernel
   change" does not hold on aarch64. x86_64 llvmpipe is ready today (RW→RX mprotect supported).

---

## 4. INTEGRATION STEPS (for the tree wave, if/when a swap is chosen)

1. **Ship-set swap** — replace in the mkfs source dir the six sonames from
   `llvmpipe-lane/stage-<arch>/usr/lib/` (libEGL, libGLESv2, libGLESv1_CM, libgbm, libgallium-25.3.6,
   gbm/dri_gbm.so) — identical sonames, clients need no relink.
2. **Add the 6 new deps** from `llvmpipe-lane/deps-<arch>/` to `/usr/lib` (with soname symlinks):
   libLLVM.so.19.1 (~147/166 MB), libstdc++.so.6, libgcc_s.so.1, libzstd.so.1, libxml2.so.2,
   liblzma.so.5. In `scripts/mkfs-f2fs-populated.py`, append them to `usr_lib_files` (the list at
   `:820`); image auto-sizes (×2 margin, `:551-553`) — no size constant to change
   (`notes/kernel-readiness-audit.md` Task 3). Cost ≈ +290/320 MB per image.
3. **libc single-instance gate** — `/usr/lib/libc.so` MUST be the same musl file as
   `/lib/ld-musl-<arch>.so.1` (m3 musl-dynamic already guarantees this). A second musl → threaded
   llvmpipe context create fails NO_MEMORY (`llvmpipe-lane/NOTES.md`).
4. **Driver select** — default megadriver still picks a software driver; set `GALLIUM_DRIVER=llvmpipe`
   in the **permanent session launch environment** to force llvmpipe over softpipe. (softpipe stays
   in the same libgallium as a fallback — so this same build can also ship as a like-for-like softpipe
   replacement by simply NOT setting the env, which is the conservative option.)
5. **aarch64 ONLY — kernel prerequisite**: land the `SCTLR_EL1.UCI|UCT` fix
   (`kernel/src/entry_aarch64.s:219-221` + `arch/aarch64/src/smp.rs:169`) BEFORE enabling llvmpipe,
   or aarch64 crashes on first JIT. x86_64 needs nothing.

---

## 5. RECOMMENDATION → **SWAP-AFTER-M5** (with one cheap experiment offered)

**Do NOT swap the ship set to chase the M5 blocker.** Reasons, in priority order:
- The M5 blocker is not a Mesa EGL-extension gap (proven: softpipe already advertises
  import/export/modifiers on the GBM path; anvil presents). The surface symptom is a cosmic-comp
  device-selection fallback fixed by `COSMIC_RENDER_DEVICE`, and the real blocker is a kernel/VFS
  dmabuf-pool issue the M5d wave already owns — Mesa cannot fix it.
- The only real Mesa delta (LINEAR `gbm_bo_create_with_modifiers`) is an *unproven* lever for the
  reallocation churn, and dropping a 147 MB libLLVM + a brand-new JIT PROT_EXEC path + an aarch64
  SCTLR kernel dependency into a live, half-diagnosed debugging effort adds risk and confounds the
  kernel wave's measurements. Wrong time.

**Adopt llvmpipe AFTER M5 lands** (via the kernel/VFS pool fix) as a hardening + perf upgrade: it is
a proven drop-in, it closes the LINEAR-modifier gap (more robust dmabuf swapchains, fewer implicit
fallbacks), and it is the M7 perf lever (5–20× softpipe on native; net win even under TCG). Bundle
the aarch64 SCTLR.UCI/UCT fix at that point.

**Optional low-cost experiment (does not disturb the aarch64 M5 line):** if the orchestrator wants to
*test* whether the modifier gap contributes to the per-frame churn, do it on **x86_64** — llvmpipe is
ready there with no kernel change. Swap the x86_64 ship set, set `GALLIUM_DRIVER=llvmpipe` +
`COSMIC_RENDER_DEVICE=226:0`, run cosmic-comp, and count PRIME exports / "Failed to submit" per
second vs the softpipe baseline. If the fd storm collapses, that is strong evidence the LINEAR
modifier support lets smithay build a reusing swapchain — and it justifies bundling the aarch64
SCTLR fix and swapping wholesale. If the churn persists, the kernel/VFS pool fix is confirmed as the
sole necessary path and llvmpipe reverts to a post-M5 perf upgrade. Either outcome is decisive and cheap.

### Hard negatives worth recording (with citations)
- **No non-LINEAR modifier support is possible from any software rasterizer here.** llvmpipe
  advertises LINEAR only (`lp_texture.c:1767,1775`); softpipe advertises none; KMS advertises none
  (`drm_device_interface.rs:1121` `DRM_CAP_ADDFB2_MODIFIERS => 0`). Correct and sufficient for a CPU
  compositor — do not expect tiled/compressed modifier paths.
- **softpipe's on-target dmabuf import/export is entirely contingent on the kernel's DRM_CAP_PRIME
  lie** (`drm_device_interface.rs:1132`). If that workaround is ever reverted, softpipe silently
  loses dmabuf on Path B while llvmpipe would not (llvmpipe never consults the kernel cap —
  `lp_screen.c:199-200`). This is a robustness argument for llvmpipe, independent of M5.
- **MESA_image_dma_buf_export is NOT available on the deviceless path for either build** (needs the
  EXPORT bit; llvmpipe deviceless gives IMPORT-only via udmabuf — `lp_screen.c:205`; empirical
  export=0 both). So swapping to llvmpipe would make the software-fallback EGL advertise *import*
  (killing the "Dmabuf import extension not available" WARN) but still not *export* — the
  `COSMIC_RENDER_DEVICE` GBM-path fix remains the correct route regardless of the Mesa build.
