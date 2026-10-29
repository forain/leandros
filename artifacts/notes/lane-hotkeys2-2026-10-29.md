# lane/hotkeys2 (2026-10-29): volume/brightness hotkey groundwork

Status: PARTIAL by design (brightness key mapping awaits the laptop key test).

- busctl: new bin `busctl` in ports/sysbus (`call` with --user/--system/-q, SIG chars s o b y n q i u x t d). build.sh stages it at usr/bin/busctl in the ship tree; mkfs packs it 0755. Gate (aarch64 COSMIC session, audio-session-test.py `AV_STEPS=probe,busctl`): VolumeUp rc=0, wpctl volume 1.00 -> 1.05.
- libudev shim: dynamic /sys/class/backlight enumerate, new_from_syspath/subsystem_sysname, sysattr reads (brightness, max_brightness, type). Empty list when the directory is absent (aarch64 session ran without errors).
- keyboard.rs: `kbd: unmapped sc e0 xx dn|up` serial line, max 10 per second. No brightness mapping guessed; after the laptop key press, map the logged codes to KEY_BRIGHTNESSDOWN/UP (224/225).
- Regress: aarch64 default PASS (Mac), x86_64 default PASS (box).
