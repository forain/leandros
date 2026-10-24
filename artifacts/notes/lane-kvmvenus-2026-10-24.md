# lane/kvmvenus 2026-10-24 (L1.14 x86_64/KVM Venus run): PARTIAL — first real in-guest run of the hostvis/BLOBWIN guards is RED

## Box sync (no push)
Recipe: scratchpad box/SYNC.md. Box worktrees on the real disk S=/run/media/forain/samsung970pro512/leandros-siblings
(root fs 97% full): $S/leandros-wave (branch lane-kvmvenus, symlink /home/forain/Projects/leandros-wave) and
$S/relibc-wave (wave-master = Mac relibc master 21c305aa); build with LEANDROS_RELIBC_DIR=$S/relibc-wave.

## FIXED: x86_64 build on a Linux host
- `shasum` (perl, macOS) is absent on the box: ports/sudo-rs/build.sh died rc=127, ports/greetd/build.sh never wrote
  greetd.srchash, run-qemu.sh's MAC hash came out empty. Now sha1sum/sha256sum first, shasum fallback
  (10ea2f86, 5b111eef; also rtl_nic/iwlwifi/i915_fw fetch, iso-codes, openrm). Mac-only scripts left alone.
- Box's shared ~/code/leandros-artifacts/greetd-lane/x86_64/greetd was stale (no `rounds=`); rebuilt.
- `build-all.sh --arch x86_64` on the box: green (main 1aa27127 + these commits).

## RED: runtests x86_64 --venus (accel=kvm), tag kvmvenus-ven
- venustest RC=6: host3d_window_spans_released (spans 2 -> 27 after 20 create/map/close cycles, never return),
  host3d_window_offset_reused, BLOBWIN_SPAN_RETURNED_AFTER_MUNMAP (30 with A mapped, 30 after both gone),
  phase3_17th_context_init_refused + phase3_17th_has_no_context (17th open got ctx_id 18, expected refusal),
  phase6_objs_zero_after_fd_close. PASS: BLOBWIN_NEW_BLOB_GETS_OTHER_OFFSET, BLOBWIN_OLD_MAPPING_KEEPS_ITS_MEMORY,
  BLOBWIN_CLOSED_TOKEN_REFUSES_MMAP, host_visible_param_matches_window, phase3_16_contexts_distinct, phase6 others.
- drmsmoke --hostvis RC=1: HOSTVIS_CREATE — virgl ctx (capset 2) RESOURCE_CREATE_BLOB HOST3D -> host resp 0x1200
  (ERR_UNSPEC). Plain drmsmoke under --venus: RC=0.
- Hypothesis (unverified): spans + phase6 objs share one cause — the DRM per-open/GEM teardown never runs on
  munmap/close in-guest (blobwin was proven only by host unit tests). Span counter: drivers/src/drm_device_interface.rs
  (VIRTGPU_PARAM_LEANDROS_HOSTVIS_SPANS). The 17th-context case looks like a missing/raised per-device context cap.
- Also seen: "[DRM] unimplemented ioctl cmd=0xC0286405" (nr 5 = DRM_IOCTL_GET_CLIENT), once per boot.

## Open
- All of the RED list above; x86_64 regress suite under KVM not run yet. Handoff: scratchpad handoff/kvmvenus.md.
