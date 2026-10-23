# lane/virglarm 2026-10-20 (aarch64 virgl: L11.1a, L11.1b, L11.7)

## L11.1b `__stack_chk_guard` (commit e1615d2d)
`scripts/musl/relink-libc.sh` now exports `__stack_chk_guard` from libc.so so
libvulkan.so.1 relocates. Evidence: `--virgl` aarch64 boot, `vktest` ->
`[PASS] vkCreateInstance -> VK_SUCCESS`, no relocation error, zero
`stack_chk` lines in the serial log. (0 physical devices is expected: virgl, not
Venus.)

## L11.1a iced/wgpu EBADF / greeter FAR=0x224: ALREADY-FIXED
Same virgl boot: the iced/wgpu cosmic-greeter came up and rendered the login
card (screenshot via VNC); serial log has 0 matches for `EBADF|FAR=0x224`.

## L11.7 VT-switch flash (commit 0a14742f)
Stale-present gate in `present_blob_fb`, per-VT last-fb record,
`drm_vt_present_fence` (called from `fb_vt_scanout_revoke`),
`drm_vt_graphics_restore` (called from tty `complete_switch`), and a generic
exit repaint safety net in `handle_ioctl`. Compile fix needed: `core` is a local
module in drm_device_interface.rs, so `core::hint::spin_loop()` became
`::core::hint::spin_loop()`. Kernel and drivers otherwise compiled clean.

Tests (aarch64 release, `LEANDROS_RUN_ID=virglarm`):
- regress: `boots failed: 0/1; overall: PASS` (no wake-count flakes).
- 5x VT round trip (greeter on VT1 <-> text VT2, `driver.py chord`, VNC
  screenshots, script: vtloop.py): text VT identical 2 s apart (0.00% diff all
  5 runs, no desktop flashing back); screenshot ~0.3 s + capture after the
  return chord vs. the original: 0.00-0.12% (clock/cursor pixels), restored,
  not stale text console. No `RESOURCE_FLUSH refused`, no panic in the log.

Caveats: the loop ran against the greeter (a static client), not a moving
client, and there was no pre-fix baseline run, so "the flash is gone" is
shown as "no flash observed in 5 trips", not as a before/after. `vt_handoff_stats`
is still not exported to a debug dump, so STALE_PRESENTS/VT_RESTORES counts are
unasserted.
