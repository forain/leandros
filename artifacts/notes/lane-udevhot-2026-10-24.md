# lane/udevhot (L11.12) - USB input hot-plug reaches the compositor - 2026-10-24

Status: FIXED (guard green aarch64).

## What changed
- `/dev/input/hotplug`: evdev server announces node add/remove so userspace sees hot-plugged input devices.
- libudev stand-in: enumerates every `/dev/input/eventN` (coldplug) and runs a monitor for add/remove, so libinput in cosmic-comp opens hot-added nodes.
- xHCI with no device at boot is now scanned and polled, so a first hot-add on an empty controller is seen.
- Per-node capture guards: `evtest2 cap <node> <secs> <churn> <go-file>` readers all start in one round trip and open their window on a go-file (usbunplugtest readers phase 12/12, broken variant proven to fail).
- event0 stays a console-only mirror for USB keyboards that have their own node.
- udevhottest check (a) now proves libinput opened the node from the evdev server's own `[EVDEV] register dev=N open=M pid=P` line with P = cosmic-comp pid (the /proc/<pid>/fd listing was unreliable). Run: `PASS (a) ... ['3']`.

## Evidence (aarch64, HVF, --virgl)
udevhottest (a)(b)(c) PASS; usbunplugtest all phases PASS; evgkeytest, evherdtest, evtest2, clickmovetest PASS; full regress see merge message.

## Left
- evtest2-based byte checks in usbunplugtest hid phase are noisy (+-48 B); counting KEY_A only would be stronger.
- x86_64 only build-verified.
- Note: evgkeytest.py/evherdtest.py must be run via `runtests.py ... host:<name>.py` (they need the harness-booted VM).
