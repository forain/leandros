# Lane kvmvenus4 (L1.14, x86_64/KVM Venus) — 2026-10-24

Status: FIXED. Continues lane/kvmvenus3 (CREATE_BLOB cmd-stream fix, FREE_CTXS getparam, desktop-off finding).

## Fixes
- RESOURCE_CREATE_BLOB submits `cmd` before the create (upstream parity); virgl HOST3D blobs were failing host-side (0x1200).
- venustest/drmsmoke "leaks" were COSMIC running alongside: runtests now boots `--venus`/`--virgl` runs of
  venustest/drmsmoke with `nvrm.desktop=0` (appended unless the caller set nvrm.desktop).
- venustest 4c `host3d_window_offset_reused`: compared the DRM mmap token, a fake offset handed out next-fit on
  purpose (differs every cycle). New getparam VIRTGPU_PARAM_LEANDROS_HOSTVIS_LAST_OFF (0x1000_000D, window page of the
  last host-visible MAP); 4c now compares that. Before: FAIL (tokens 0x100000011000, 0x100000032000, ...; window page
  0 every cycle per trace). After: PASS, window page 0 x20.
- drmsmoke HOSTVIS_MAP_OPAQUE/PARTIAL/STALE_REFUSED: test bug. PIPE_RESOURCE_CREATE used bind VIRGL_BIND_STAGING
  (1<<19); vrend gives staging buffers no GL storage, so the blob creates but RESOURCE_MAP_BLOB has nothing to map
  (0x1200). Now bind VERTEX_BUFFER with MAP_PERSISTENT|COHERENT (GL_ARRAY_BUFFER + glBufferStorage on the host).
- HVDBG span trace removed.

## Evidence (box, QEMU 11.1.1, virglrenderer 1.3.0, KVM)
- `runtests x86_64 --venus /bin/venustest "/bin/drmsmoke --hostvis"`: both 1/1 PASS (cmdline nvrm.desktop=0 automatic).
- x86_64 regress (KVM): overall PASS. aarch64 regress (Mac): overall PASS.

## Open
- phase3 opens up to 256 contexts; the host render server logs "too many workers" past its own limit (guest test still
  passes). Consider capping the probe count.
- Box note: a second box worktree needs the sibling repos (../brush, ../coreutils, ../leandros-artifacts, ...)
  reachable from its parent dir, or login fails with "exec failed" (no /bin/brush). Also set LEANDROS_VNC_PORT
  per parallel --venus VM (default 5909 clashes).
