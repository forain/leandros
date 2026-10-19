# Plan: GPU video decode (VA-API) on the laptop — 2026-10-18

Target: HP EliteBook 840 G5, KBL-R GT2 (UHD 620, gen9.5). Firefox 136.0.4 (Alpine 3.21), x86_64 only.
Today: `ports/firefox/leandros-prefs.js` sets `media.hardware-video-decoding.enabled=false` and
`media.ffmpeg.vaapi.enabled=false`. During YouTube playback total CPU is about 310 %.

## Findings

### Which VA driver: iHD (intel-media-driver), not i965
- KBL is supported by both. KBL HW decodes H.264, HEVC 8/10, VP8 and VP9 profile 0/2. **It has no AV1 decode.**
- **i965 (intel-vaapi-driver) needs more kernel work than iHD.** It goes through libdrm_intel, which uses
  **relocations** (we return EINVAL when `relocation_count != 0`, `uapi_abi.rs::check_exec_object`) and the
  legacy `GEM_MMAP` (we return ENODEV). Upstream is dead, and FreeBSD deprecated it.
- **iHD (current master) fits our softpin-only uAPI:**
  - The user setting "Enable Softpin" defaults to 1 (`mos_user_setting_specific.cpp`), which calls `mos_bufmgr_enable_softpin`.
  - It uses `GEM_MMAP_OFFSET` when `MMAP_GTT_VERSION >= 4` (we answer 4).
  - It uses SET/GET_TILING, GEM_BUSY/WAIT, CONTEXT_CREATE(_EXT), QUERY ENGINE_INFO and GETPARAM (EU/SUBSLICE_TOTAL,
    HAS_BSD/HAS_VEBOX/HAS_EXEC_SOFTPIN). We already provide all of these.
  - On gen9 it submits decode with **`I915_EXEC_BSD | I915_EXEC_BSD_RING1`** (`mos_os_specific.c`).
  - Decode needs no HuC or GuC (on KBL, HuC is only for encode BRC).
  - ⚠ Unverified: whether Alpine 3.21's iHD 24.x already defaults to softpin. Phase 2 checks this.
- **What our i915 lacks: the VCS (BSD) engine.**
  - `hw.rs` describes vcs0 (mmio 0x12000, GRDOM_MEDIA, irq bank 1) and vecs0.
  - `exec.rs` (NENG=2) and `uapi.rs` (`ENGINES = [rcs0, bcs0]`) drive only rcs0 and bcs0. As a result HAS_BSD=0,
    ENGINE_INFO has no class 2, and a legacy `EXEC_BSD` is refused by `legacy_ok`.
  - vecs0 is not needed for plain decode with dmabuf export.

### Userland
- **libva**: already shipped. Alpine's libavcodec closure pulls in libva.so.2, libva-drm.so.2 and libva-x11.so.2
  (`out/*/CLOSURE.txt`).
- **Missing userland**: `intel-media-driver` (iHD_drv_video.so) plus `gmmlib` (libigdgmm.so.12), x86_64 only. For
  tests, also add `libva-utils` (vainfo) and the `ffmpeg` CLI.
- **Mesa**: nothing for decode. iris already imports the NV12 Y-tiled dmabufs into WebRender.
- **FFmpeg**: shipped.
  - System libavcodec.so.60 (FFmpeg 6.1, built with vaapi) handles H.264/HEVC.
  - The bundled libmozavcodec (ffvpx) handles VP8/VP9/AV1. Its VA-API path is gated by `media.ffvpx-hw.enabled`;
    check its default in 136.
  - `vaapitest` is staged in /usr/lib/firefox.
- **RDD sandbox**: already off (`MOZ_DISABLE_RDD_SANDBOX=1` in `ports/firefox/firefox.sh`), so no pref is needed.
- **Prefs to flip, in phase 6**:
  - `media.hardware-video-decoding.enabled=true`
  - `media.ffmpeg.vaapi.enabled=true`
  - `media.hardware-video-decoding.force-enabled=true`, if the vaapitest probe blocklists us.
  - Env `LIBVA_DRIVER_NAME=iHD`, so that libva does not fall back to i965.

### Cheaper first win (do this first)
- YouTube serves **AV1** to Firefox when it can. dav1d in software costs far more than VP9, and much more than H.264.
- The cheapest win is `media.av1.enabled=false`, which makes YouTube serve VP9. It is needed anyway, because KBL cannot
  decode AV1 in hardware.
- Optional: `media.mediasource.vp9.enabled=false` forces H.264 (YouTube caps H.264 at 1080p).
- Also check that decode is multi-threaded and where the parent's ~96 % goes. If it is frame upload or compositing,
  that is a separate cost which only zero-copy dmabuf (VA-API) removes.

## Riskiest unknown
Does iHD 24.x on musl initialise on our i915 and stay inside our uAPI? The risks are:
- relocations being used instead of softpin;
- an unknown GETPARAM or context param that it treats as fatal;
- C++ runtime or musl breakage.

Phase 2 answers this with no kernel work: vainfo plus the ioctl trace. Do it before any VCS kernel work.

## Phases (each one lane, under an hour)

**P0: measure (no code)**
- On the laptop, during YouTube 1080p:
  - Read the codec in Stats for nerds (av01 / vp09 / avc1).
  - Read the decoder row in about:support "Media".
  - Take a per-thread CPU split: diff `/proc/<pid>/task/*/stat` of RDD and the parent; look for dav1d / FFmpeg
    worker threads and the WR / Renderer threads.
- Test: a note with the codec, the thread count and the top 5 threads by CPU.

**P1: codec prefs (cheap win)**
- Add `media.av1.enabled=false` to leandros-prefs.js.
- A/B against H.264 (`media.mediasource.vp9.enabled=false`).
- Test: YouTube 3/3 on the laptop. Total CPU against the 310 % baseline. Both QEMU arches still play.

**P2: the riskiest unknown (x86_64 userland only)**
- Stage intel-media-driver, gmmlib, libva-utils and the ffmpeg CLI (`dlopen-in-alpine.sh` / build.sh).
- Run `LIBVA_DRIVER_NAME=iHD LIBVA_MESSAGING_LEVEL=2 vainfo` on the laptop with i915 ulog level 2.
- Test: the ioctl trace is captured. Expected: init succeeds or fails only on HAS_BSD=0 / no class 2. Any
  `relocation_count`, GEM_MMAP or USERPTR error means rethinking the plan (an iHD version pin, or adding
  relocations).

**P3: kernel, vcs0 bring-up**
- In `exec.rs`, extend `DESCS`/NENG to vcs0:
  - execlists at 0x12000 with a 2-page context image;
  - MOCS for VCS0 at 0xC900;
  - GT IIR bank 1;
  - media forcewake;
  - GRDOM_MEDIA reset in the hang check.
- Add an in-kernel MI_STORE_DWORD self-test on vcs0, as `gt.rs` does for bcs0.
- Test: the laptop boot log shows vcs0 store ok. QEMU x86_64 and aarch64 regress green (fake mode is unaffected).

**P4: kernel, uAPI exposure**
- Add (2,0) to `uapi.rs` ENGINES, which gives HAS_BSD=1 and ENGINE_INFO class 2.
- Accept `EXEC_BSD` (with the BSD_RING1 bits) in `legacy_ok`.
- Add rps `exec_vcs` / `busy_vcs`; check softcs fake mode for vcs.
- Test: an i915-tests case submits a store batch through legacy `EXEC_BSD|RING1` and through an engine map of
  class 2, then reads back the value. QEMU both arches green.

**P5: decode correctness**
- Run vainfo and list the VLD entrypoints for H264/HEVC/VP9.
- Run `ffmpeg -hwaccel vaapi -i clip.{mp4,webm} -vf hwdownload,format=nv12 -f framemd5 -` and compare with the
  software framemd5.
- Test: the md5s are identical for an H.264 clip and a VP9 clip, and `busy_vcs` > 0.

**P6: Firefox on**
- Flip the prefs and env above. Keep AV1 off.
- Test:
  - about:support shows hardware decoding.
  - `MOZ_LOG=FFmpegVideo:5` shows a VA-API decoder and dmabuf surfaces.
  - YouTube 3/3, and the blank-screen play test (blankplay) still passes.
  - Total CPU is far below the P1 number.
  - aarch64 QEMU falls back to software and still plays.
