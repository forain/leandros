# `vkrender` — M3 step 2: the first GPU submission from LeandrOS

Source work only. Nothing here was built or run; the repo was not modified.
Design: `~/code/leandros-artifacts/notes/m9-m3-vulkan/m3-vulkan-design.md` §1.

| File | Lines | What |
|---|---|---|
| `vkrender.c` | 1679 | the program |
| `shaders/fillpattern.comp` | 32 | subtest 1 GLSL |
| `shaders/triangle.vert` | 39 | subtest 2 GLSL |
| `shaders/triangle.frag` | 19 | subtest 2 GLSL |
| `build-vkrender-alpine.sh` | 118 | in-container build, vktest idiom |
| `staging-change.md` | — | the exact `mkfs-f2fs-populated.py` edit, described not applied |
| `README.md` | — | this |

`vkrender.c` was syntax-checked with `clang -std=c11 -Wall -Wextra
-fsyntax-only` against Mesa 25.3.6's `include/` in **both** configurations
(with a stub `vkrender_spv.h`, and with `-DVKRENDER_NO_SHADERS`): zero warnings,
zero errors. That is the only compilation that happened — it produced no object
file and touched nothing outside this directory.

---

## What each subtest proves

### `s0_*` — shaderless. The one that matters.

`vkCmdFillBuffer(dev_buf, 0xDEADBEEF)` → buffer barrier → `vkCmdCopyBuffer` into
a `HOST_VISIBLE|HOST_COHERENT` buffer → barrier to `HOST_READ` → **one
`vkQueueSubmit`** → **one `vkWaitForFences`** → assert all **65536** words.

No shader, no image, no render pass, no descriptor set, no pipeline. If this
hangs, the Venus ring is the only suspect — which is the entire reason it is
subtest 0. This lane has been bitten in `vn_ring`'s relax loop twice already
(`75b32e3` `CLOCK_MONOTONIC` granularity, `fb398c7` `nanosleep` truncation),
both only under sustained ring traffic, and a 256 KiB fill+copy is an order of
magnitude more ring traffic than the enumeration `vktest` does today.

The host buffer is **CPU-poisoned to `0xA5A5A5A5` before every submit**, so
"the GPU did nothing" and "the GPU wrote zeros" are different observable
outcomes, and the failure message says which one it saw.

Also proven here, for free: an *app*-allocated `HOST_VISIBLE` blob actually
maps into the guest. Until now only Mesa's own ring shmem has taken that path.

### `s1_*` — compute

64 workgroups × 64 invocations write `(i * 2654435761) ^ 0x9E3779B9` into a
storage buffer, which is then copied to host memory. The value varies per
index, so **no memset, no zero-init and no `vkCmdFillBuffer` can produce it**.
`dev_buf` still holds subtest 0's `0xDEADBEEF` at this point, so the three
interesting failures are distinguishable by the observed word alone, and the
error message names them: poison survived (copy never landed), `0xDEADBEEF`
survived (copy ran, shader did not), zeros (shader wrote nothing).

### `s2_*` — graphics

Clear to blue + one triangle in red into a 256×256 `R8G8B8A8_UNORM`
`VK_IMAGE_TILING_OPTIMAL` colour attachment in `DEVICE_LOCAL` memory, then
`vkCmdCopyImageToBuffer` into the host-visible buffer. Four assertions:

- **`s2_pixels`** — 13 named coordinates. Triangle is apex (128,32), base
  (32,224)–(224,224). Four corners, five points outside (above the apex, left
  and right of the edges at a known row, below the base), four points inside
  (centroid plus three more). These assert *positions*, not "something
  changed": a flipped y-axis, a wrong viewport or a half-scale triangle all
  fail specific named points.
- **`s2_coverage`** — the red-pixel count against the **analytic triangle area**
  (½·192·192 = 18432 px, tolerance ±600 to cover the fill rule on the ~622-px
  perimeter). This is the assertion that geometry errors cannot satisfy by luck.
- **`s2_no_intermediate_pixels`** — with blending off and 1 sample, every pixel
  must be exactly `FF 00 00 FF` or `00 00 FF FF`. Reported separately so it can
  be waived without hiding the two above.
- **`s2_checksum`** — FNV-1a over all 262144 bytes, printed. Set
  `VKRENDER_EXPECT_CHECKSUM=0x…` from the first known-good run to turn it into a
  regression assertion. Deliberately **not** hardcoded: nobody has run this yet
  and a value nobody has observed is not an assertion, it is a guess.

Also logged along the way (design §6, free): every memory type's property flags
and heap plus the chosen index (Q3), whether `VK_KHR_swapchain` is advertised
(Q6), and whether `VK_EXT_external_memory_host` is advertised (Q5).

---

## What a pass looks like

Output idiom matches `drmsmoke` / `venustest` so the wave harnesses parse it:
`"<name>: PASS"`, `"<name>: FAIL"`, `"<name>: TIMEOUT (waited N ms)"`,
`"<name>: SKIP (why)"`, and a final marker

```
--- vkrender done, failures = 0, skipped = 0 ---
```

Exit code is the failure count (clamped to 125), or **126 on timeout**.
A clean run ends with, in order:

```
s0_record: PASS
s0_submit: PASS
s0_verify: PASS
s1_… : PASS  (×7)
s2_… : PASS  (×11)
vkDestroyDevice: PASS
vkDestroyInstance: PASS

--- vkrender done, failures = 0, skipped = 0 ---
```

`stdout` is set unbuffered at entry. If a submit hangs, everything printed so
far is already on the serial line — that is the difference between a
diagnosable hang and a silent one.

### Timeouts

`vkWaitForFences` is **never** called with `UINT64_MAX`. Default timeout 20000 ms
(aarch64/TCG is slow), overridable with `--timeout-ms=N` or
`VKRENDER_TIMEOUT_MS`. On timeout vkrender prints a distinct `TIMEOUT` line,
sets exit code 126, and **abandons the run without tearing anything down** —
destroying objects while the queue may still be executing is undefined
behaviour and would replace a diagnosable hang with a crash.

---

## Build

Same container idiom as `vktest`
(`~/code/leandros-artifacts/venus-lane/build-vktest-alpine.sh`): Alpine 3.21,
native `cc`, `-fno-stack-protector -U_FORTIFY_SOURCE`, link the local
`ssp_guard.o`, `-ldl`, then `patchelf --replace-needed libc.musl-$ARCH.so.1
libc.so`.

```sh
ART=$HOME/code/leandros-artifacts
mkdir -p $ART/venus-lane           # already exists
cp -r <this dir>/{vkrender.c,shaders,build-vkrender-alpine.sh} $ART/venus-lane/
cd $ART/venus-lane                 # ssp_guard.c is already here

docker run --rm --platform linux/arm64 \
  -v $ART/llvmpipe-lane/src/mesa/include:/work/vkheaders:ro -v $PWD:/out \
  alpine:3.21 sh /out/build-vkrender-alpine.sh aarch64
docker run --rm --platform linux/amd64 \
  -v $ART/llvmpipe-lane/src/mesa/include:/work/vkheaders:ro -v $PWD:/out \
  alpine:3.21 sh /out/build-vkrender-alpine.sh x86_64
```

Output: `$ART/venus-lane/stage-<arch>/usr/bin/vkrender` — the same directory
`mkfs-f2fs-populated.py` already reads `vktest` from, which is why the staging
change stays at three lines.

Trust the final `=== rc=0 arch=… vkrender ===` line, not the log body.

### Assumptions in the build recipe that could not be verified here

1. **The `/work/vkheaders` mount path.** `build-vktest-alpine.sh` documents the
   mount but the host-side `docker run` that produced `vktest` is not archived
   anywhere in `leandros-artifacts`. `$ART/llvmpipe-lane/src/mesa/include` is
   the only Mesa `include/` tree in the artifacts repo and it contains
   `vulkan/vulkan_core.h` and `vulkan/vk_icd.h`, so it is almost certainly what
   was mounted — but it is inferred from the file layout plus the `llvmpipe-lane`
   `NOTES.md` invocation pattern, not read from a recorded command.
2. **Alpine 3.21 packages `glslang` with a `glslangValidator` binary.** Not
   verifiable without running `apk`. The script falls back to `shaderc`/`glslc`
   and then to `-DVKRENDER_NO_SHADERS`, so a wrong guess degrades instead of
   failing — but if both packages are missing you get subtest 0 only.
3. **`glslangValidator --vn` emits `const uint32_t <name>[] = {…};`.** This is
   its documented behaviour and it is what makes `sizeof(spv_fill_comp)` work.
   The script greps the generated header for the SPIR-V magic `0x07230203` as a
   cheap sanity check; if `--vn` ever changed shape, that grep and then the C
   compile would fail loudly rather than silently.
4. **Dispatchable handles work without a loader beyond `VkDevice`.** `vktest`
   proves `VkInstance` / `VkPhysicalDevice` / `VkDevice` round-trip fine when
   the ICD's own handles are handed straight back to the ICD. `vkrender` adds
   `VkQueue` and `VkCommandBuffer` to that set. Same mechanism, never exercised.
   If it were wrong it would fail immediately and loudly at
   `vkAllocateCommandBuffers` or the first `vkCmd*`, not subtly.
5. **`-I/out`** is added so the generated `vkrender_spv.h` is found next to
   `vkrender.c`. Harmless if unused.

## Staging

See `staging-change.md`. Three lines after
`scripts/mkfs-f2fs-populated.py:610-612`. No new DT_NEEDED closure — `vkrender`
links exactly what `vktest` links.

---

## The SPIR-V question — stated plainly

**The SPIR-V is NOT embedded. It is generated at build time from GLSL that
ships here, and the exact command is in the build script.**

There is no shader compiler on this Mac (`glslangValidator`, `glslc`,
`spirv-as`, `shaderc`, `naga` — all absent; no Vulkan SDK). I could hand-write
SPIR-V binary words, but I could not validate them, and hand-assembled words
presented as correct would be exactly the kind of unverified claim this lane
keeps getting burned by. So:

- `shaders/*.{comp,vert,frag}` are the source of truth, with the exact
  regeneration command in each file's header comment.
- `build-vkrender-alpine.sh` compiles them in the container and emits
  `vkrender_spv.h` with `spv_fill_comp[]`, `spv_tri_vert[]`, `spv_tri_frag[]`.
- If no compiler is available the build defines `VKRENDER_NO_SHADERS`, and
  **subtest 0 — which needs no SPIR-V at all — is the shippable core**, with
  subtests 1 and 2 reporting `SKIP` rather than `FAIL`.

To generate them by hand outside the container:

```sh
glslangValidator -V --vn spv_fill_comp -o comp.h shaders/fillpattern.comp
glslangValidator -V --vn spv_tri_vert  -o vert.h shaders/triangle.vert
glslangValidator -V --vn spv_tri_frag  -o frag.h shaders/triangle.frag
cat comp.h vert.h frag.h > vkrender_spv.h
```

---

## `--present`

Off by default, so the default run is headless and suite-safe.

`vkrender --present [--present-hold-ms=8000]` blits subtest 2's rendered pixels
(centred, on a dark grey field) into a DRM dumb buffer and scans it out. The
sequence is lifted from `userland/drmsmoke/src/main.rs:362-425`, which already
proves it on both arches: `GETRESOURCES → GETCONNECTOR (two-pass) → CREATE_DUMB
→ MAP_DUMB → mmap → blit → ADDFB2 → SETCRTC → DIRTYFB`, then hold so a QMP
`screendump` can catch it. Channel order is swapped on the way in:
`DRM_FORMAT_XRGB8888` is B,G,R,X in memory, our source is R,G,B,A.

> **It cannot run while cosmic-comp holds the CRTC.** LeandrOS never gates
> `SETCRTC` on DRM master (`drm_device_interface.rs:1436-1439`, "Root
> single-seat: master is not gated"), and there is one CRTC (id 1) and one
> connector (id 1). vkrender would simply take the screen and the two would
> fight over it. **Stop COSMIC first.** The program prints this warning before
> it opens the card.

One implementation note: musl declares `int ioctl(int, int, ...)` and
sign-extends any request with bit 31 set. That is fine here because the kernel
masks the request with `0xFFFF_FFFF` before dispatch, for exactly this reason —
`kernel/src/syscall.rs:5968`, whose comment records that this is what made
`anvil`'s `gbm_bo_get_fd` fail while `drmsmoke` (Rust, zero-extended) succeeded.

---

## Where this can be verified

**On the Mac:** the container build of `vkrender` (a cross-arch container
build, not a host-EGL thing), the `mkfs-f2fs-populated.py` staging edit, and an
image build. Also a non-Venus QEMU run to confirm the staged binary is present
and that it fails cleanly at `dlopen`/`vkCreateInstance` when there is no Venus
device — a useful negative control.

**Only on the Linux box** (`forain@172.16.158.150`, EndeavourOS, virglrenderer
1.3.0, QEMU 11.0.1): **everything that touches Venus.** macOS has no EGL, so
`virtio-gpu-gl-pci,venus=on` cannot initialise there. This is a host-platform
fact, not a code defect. Concretely: every `s0`/`s1`/`s2` result, all the
memory-type and extension logging, and `--present`.

Reference device line (verbatim from the M2 passing wave):

```
-device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G -display egl-headless
```

⚠ `-nographic` silently overrides `-display` and kills Venus with no error.

---

## Failure modes I expect first, and what each would mean

1. **`s0_submit: TIMEOUT`** — most likely, and the reason subtest 0 is
   shaderless. The fence never signals: the submission is stuck in Mesa's
   `vn_ring` relax loop. There is no shader, image, render pass or descriptor
   involved, so the ring is the only suspect. *Next step:* rerun with
   `VN_DEBUG=init,result`, and grep the serial log for
   `EXECBUFFER: fields asked for but NOT honoured` — `sim_submit` passes
   `bo_handles`/`num_bo_handles` and `ring_idx`, both now implemented, so the
   expected answer is silence and anything printed is the bug. Compare the
   `vkQueueSubmit returned in N ms` and `fence signalled after N ms` lines
   vkrender prints for every submit.

2. **`s0_verify: FAIL` with the poison intact (`0xA5A5A5A5`)** — the submit
   completed and the fence signalled, but no GPU write reached the mapped
   pages. That is a *host-visible blob mapping* bug, not a ring bug: the
   guest's view of the blob is stale or points elsewhere. Check the
   `[DRM] host-visible blob mapped: res=… map_info=…` serial line for the app
   allocation and compare it to the `vkMapMemory took N ms -> 0x…` line.
   A `0xDEADBEEF`-shaped partial failure (some words right, some poison) would
   instead point at the 64 MiB `MAX_BLOB_BYTES` / 256 `MAX_HOSTVIS_SPANS` caps.

3. **`vkMapMemory_host: FAIL` or a many-second `vkMapMemory took …`** — the
   app-scale `HOST_VISIBLE` allocation. Slow is *expected*, not a hang: Mesa's
   own comment (`vn_device_memory.c:439-450`) says the first map blocks until
   the renderer injects the pages. The logged memory-type table immediately
   above tells you whether we picked a sane type; Venus normalises these
   (strips `HOST_VISIBLE` from non-coherent types, force-adds `HOST_CACHED` to
   the first coherent one), so `HOST_VISIBLE|HOST_COHERENT|HOST_CACHED` should
   exist and be what we chose.

4. **`s1_compute_pipeline: FAIL` / a very long `vkCreateComputePipelines`** —
   first real shader compile on the host. Venus forwards it, so this is host
   RADV work, but it is also the first *large* `vn_cs` payload in the other
   direction. If subtest 0 passed and this hangs, the ring is exonerated for
   small payloads and the suspect is payload size.

5. **`s2_no_intermediate_pixels: FAIL` while `s2_pixels` and `s2_coverage`
   pass** — cosmetic and waivable. It would mean the readback contains a third
   colour where none should exist; the most plausible causes are a format or
   swizzle mismatch on the copy path rather than a rasterization error. The
   error message prints the first offending coordinate and its four bytes.

6. **`s2_pixels` failing only the *inside* points while corners pass** — the
   clear landed but the draw did not. That separates "render pass works" from
   "pipeline/vertex stage works", which is exactly why the corner and interior
   assertions are separate names.
