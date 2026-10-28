# lane lapdispC1: two-CRTC DRM core with a stub pipe B (2026-10-27)

Status: DELIVERED (design-lapdisp2 Lane C1). No hardware touched.

## What changed
- `drivers/src/gpu_kms.rs`: `KmsPipe2` (modeset/disable/queue/event/busy/wait_idle/cursor/mode, plus `dumb_ok`),
  `register_pipe2`, `pipe2()`, `extra_crtcs()`, `pipe2_mask(idx)`. Pipe A's `KmsBackend`/`KmsFlip` untouched.
- `drivers/src/drm_device_interface.rs`:
  - ids `DRM_CRTC2_ID=2`, `DRM_PLANE2_ID=32`, `DRM_CURSOR2_PLANE_ID=33` (33 only when the pipe has a cursor half).
  - GETRESOURCES (crtcs [1,2]), GETPLANERESOURCES, GETPLANE (possible_crtcs A=1, B=2), GETENCODER (extra encoder
    possible_crtcs = `pipe2_mask`, crtc = bound CRTC), GETCONNECTOR (extra encoder_id when bound), GETCRTC for CRTC 2.
  - `CRTC_ACTIVE[2]`, `CRTC_MODE_BLOB[2]`, `EXT_CONN_CRTC[EXT_MAX]`, `PLANE_B_CRTC[2]`; per-object CRTC_ID/ACTIVE/MODE_ID readback.
  - ATOMIC: per-CRTC ACTIVE/MODE_ID, extra connector -> CRTC 2, planes 32/33; validation (plane only binds its own
    pipe's CRTC, connector 1 only CRTC 0/1, an extra only 0/2, at most one connector per CRTC, active CRTC needs a
    connector + valid mode blob, plane on an inactive CRTC refused, fb must resolve); modeset gating per CRTC;
    pipe-B commit through the hooks; a commit touching only B skips pipe A's busy check, event and cursor path;
    flip events carry `crtc_id=2`.
  - SETCRTC on CRTC 2 = EINVAL; legacy PAGE_FLIP on CRTC 2 goes to the pipe-B hooks.
- `drivers/src/i915/pipeb_stub.rs` (+ call in `kms.rs` after `outputs::init`): `i915.fake` + `i915.pipeb_stub=1`
  registers a stub pipe B; flips complete from a 60 Hz kernel thread; logs `drm: 2 CRTCs (pipe B: stub)` and
  `drm: CRTC 2 modeset WxH@R conn 0x100`.
- `userland/drmsmoke`: `--crtc2` mode: CRTC_ENUM, ATOMIC_CRTC2 (TEST_ONLY, commit, 10 concurrent A+B flips, every
  event carries its own crtc_id), ATOMIC_BAD (eDP -> CRTC 2 and a second extra -> CRTC 2 both EINVAL),
  ATOMIC_CRTC2_OFF. runtests suite `i915crtc2` (cmdline `i915.fake=2 i915.kms=1 i915.extout=1 i915.pipeb_stub=1`).

## Decisions / deviations
- Default behaviour: every new behaviour is gated on `extra_crtcs() > 0` (only true with a registered pipe B), including the
  connector-1 CRTC_ID readback fix, so virtio-gpu, i915 single pipe, NVIDIA and v3d see unchanged DRM output.
- No WAIT_VBLANK ioctl exists in this DRM core, so the HIGH_CRTC/SECONDARY handling in the design has nothing to attach to.
- `pipe2_mask(idx)` is separate from lane lapdispA's planned `KmsOutputs::crtc_mask`; on merge, make GETENCODER use
  `crtc_mask(i) | pipe2_mask(i)` (or have i915 return `pipe2_mask`).
- The stub accepts plain dumb framebuffers (`dumb_ok`); a real pipe (C2) needs backend tokens.
- Needs `i915.extout=1` until lane A flips the default (extras only exist then).

## Evidence
- aarch64 HVF QEMU `i915crtc2`: CRTC_ENUM, ATOMIC_CRTC2 (nb=10 pipe-B events, na=10 pipe-A events), ATOMIC_BAD,
  ATOMIC_CRTC2_OFF all PASS; first run showed ATOMIC_BAD FAIL when the test bound nothing first (test bug, fixed).
- Box farm aarch64 full regress (pre lane-A merge, 5f8f2629): all suites RC=0, `overall: PASS` (first attempt hit ENOSPC on the box
  mid-run, rerun clean). x86_64 KVM `--gpu virgl --suite drm`: PASS. No COSMIC desktop boot was run (no suite for it; the virgl
  path only needs `extra_crtcs()==0`, which is the default).
- After merging lane A (conflict in GETENCODER, resolved: `ext_crtc_mask(i)` now ORs in `pipe2_mask(i)`), local aarch64 HVF
  `i915crtc2` 8/8 repeats PASS. The earlier "timeouts" were UART interleaving of a kernel log line with the test's final
  marker, removed (no log on disable).
- `i915fake` `ddb_plane_whole_pipe_0_860` / `ddb_cursor_top_32_860_892` fail on main too (verified on a clean main build):
  not caused by this lane.
