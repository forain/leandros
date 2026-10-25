# lane/mamesetmode (openbugs #56 remainder) - FIXED

Cause: kernel, not MAME. Private DRM_IOCTL_SET_MODE (0x1001) -> ModeSet::set_display_mode
-> DrmDevice::atomic_commit. The plane state of a pure mode set has fb_id None, so
perform_software_scaling returned InvalidParameter -> DriverError::Io -> EPERM (errno 1)
after the CRTC mode was already applied. Master gating was fine (implicit grant worked).
DOOM hits the same EPERM but only checks for EACCES, so it never logged it.
Fix: drivers/src/drm/device.rs atomic_commit skips plane states without a framebuffer.
Guard: mamesession.py --vt gate mame-vt-setmode fails on any "SET_MODE failed" line.
Evidence (aarch64 QEMU): before fix mame-vt-setmode pass=false, errors=["DRM SET_MODE failed (errno 1)"];
after fix pass=true, mame-vt-audio pass; aarch64 regress overall PASS. No ../mame change needed.
