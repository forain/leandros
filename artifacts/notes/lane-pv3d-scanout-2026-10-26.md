# lane/pv3d-scanout 2026-10-26 (openbugs #98 phase E): Pi 5 scanout of v3d BOs

Status: DELIVERED (code + host tests). Pixels on the HDMI panel are pending a Pi boot that also has
phase C (SUBMIT_CL, lane pv3d-cl) and the v3d arming (phase F).
Design: `artifacts/notes/lane-piv3d-design-2026-10-26.md`, phase table row E.

## What landed
- `drivers/src/kms_v3d.rs` (new)
  - Pure, host-tested: `stride_copy(src, src_pitch, dst, dst_pitch, row_bytes, rows)` (one block
    copy when the pitches match, else row by row; whole rows only, padding untouched), `clip` (top-left,
    no scaling, clipped to the screen), `flip_log_due` (log flips 1-8, then powers of two).
  - `aarch64 + rpi5` only: `present(src_phys, w, h, src_pitch)`: `dc ivac` over the BO's HHDM alias
    (the GPU writes it through the non-coherent V3D MMU; nothing dirties that alias after
    `mmu::map_bo`'s clean, so invalidating loses nothing), the stride copy into the firmware framebuffer
    at the firmware's pitch, `dc cvac` over the destination, and the `V3D_FLIPS` counter with
    `[kms] v3d flip N WxH pitch=<bo>-><fw> rows=R`.
- `drm_device_interface.rs` `v3d_framebuffer` (ADDFB2): on rpi5, a linear XRGB8888 v3d BO with
  pitch == w*4, offset 0 and no modifier becomes a framebuffer with `v3d_scanout = true` (new
  `DrmFramebuffer` field) and no virtio resource. The same validation as before refuses anything else.
- `drm/device.rs`: `perform_software_scaling` (SETCRTC, PAGE_FLIP, atomic) and `present_damaged`
  (DIRTYFB) send a `v3d_scanout` framebuffer to `kms_v3d::present` (rpi5 cfg only). Before this, the
  fast path copied `w*h` pixels contiguously and ignored the firmware pitch, and read the BO through a
  cacheable alias with no invalidate.
- QEMU (virt, raspi4b): the field is always false and the hooks are compiled out; byte-identical path.

## Evidence
- Host: `cd drivers/v3d-tests && cargo test --target aarch64-apple-darwin`: 31/31 (26 old + 5 new:
  tight 7680 -> firmware 8192 with padding untouched, equal pitch, padded -> tight, short buffers stop
  at whole rows, clip + log cadence). Mutating the destination row offset to use the source pitch
  (the old behaviour) fails 3 of them.
- `scripts/check-rpi5.sh`: rpi5 and raspi4b clean, no new warnings (a deliberate error in the rpi5
  module was caught, so the hardware half is really compiled).
- aarch64 regress on the box: see the merge section below.

## Next Pi boot
- Nothing new at boot (the path is only reached by a v3d client).
- Running kmscube on v3d (needs phases C+F): `[kms] v3d flip 1 1920x1080 pitch=7680->7680 rows=1080`,
  then flips 2..8, 16, 32, ... The right-hand pitch is the firmware's (`[MBOX] pitch=`); 7680 when
  `v3d_linear_match=1`. Screen size is whatever the firmware mode is.
- Failure shapes: `rows=` less than the height means the BO or the firmware framebuffer was shorter
  than its claimed size. A torn or stale image with the right counter points at cache maintenance
  (the HHDM attribute of the firmware framebuffer). No line at all with kmscube running means ADDFB2
  did not take the v3d path (check `kmscube` stderr for ADDFB2 errors).

## Later: direct scanout without the copy (desk notes)
The copy costs one 8 MB read + write per frame plus maintenance. Dropping it needs the display
engine to read the BO itself:
- Firmware KMS (mailbox SET_PLANE 0x48015 etc., Linux `vc4-fkms-v3d`) is the cheap path on Pi 4, but
  Pi 5 firmware is documented as full-KMS only (fkms is not supported there). Treat as unlikely; a
  one-tag probe (does SET_PLANE answer?) would settle it.
- Panning trick: allocate the firmware framebuffer with virtual height 2h, flip with
  SET_VIRTUAL_OFFSET (0x48009). This only helps if the GPU renders into the firmware pages, i.e.
  CREATE_BO for the scanout surface hands out (and the V3D MMU maps) the firmware framebuffer's halves.
  Needs `v3d_linear_match=1` and is a hack around GBM's allocation; it still has no vblank event.
- Real HVS (the Linux `vc4` path, gen 6 on BCM2712): `hvs@107c580000` (0x1a000; its `interrupts = <0x02 0x09 0x10>`
  is not a plain GIC SPI triple, so check its interrupt-parent before wiring the EOF IRQ), pixelvalves `7c410000` (SPI 0x65) / `7c411000` (SPI 0x6e), MOP/MOPLET
  for writeback. Keeping the firmware's mode and timing, a minimal driver only has to: write a
  one-plane display list into the HVS dlist RAM (control word with XRGB8888 format + unity scaling,
  position, size, a 40-bit pointer to the BO's PA, pitch), point the display's list pointer at it at
  the next end-of-frame, and take the HVS EOF interrupt as the vblank for PAGE_FLIP events. The
  firmware's own dlist must be read first so the console can be restored on release. Port source:
  Linux `drivers/gpu/drm/vc4/vc4_hvs.c` / `vc4_plane.c` (the `VC4_GEN_6_C/D` paths). The BO's PA must be
  reachable by the HVS (40-bit on BCM2712) and the BO must stay pinned while it is on screen.
- Recommended order: probe SET_PLANE once (cheap); if refused, port the gen-6 HVS plane update.
