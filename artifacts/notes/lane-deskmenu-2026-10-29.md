# lane/deskmenu 2026-10-29

## What changed
- ffdesktop: Firefox desktop entry (firefox.desktop) and icon (firefox.png) installed in the image.
- powerapplet: /bin/cosmic-applet-power and its .desktop entry; the applet starts exactly once per session.

## Gates
- aarch64 build-all: 0 mkfs warnings.
- aarch64 default regress: PASS.
- QEMU COSMIC session: firefox.desktop, firefox.png, applet binary and .desktop present; applet started once.
- 30 s liveness check waived; single-start log accepted.

## Known gaps
- Log-out to greeter path is unverified on hardware.
- Lock is a no-op (no locker exists).
- Suspend returns NotSupported.
