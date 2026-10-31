# lane/lapallm2 - follow-up to the all-modules laptop trial - 2026-10-31

Trial kernel 11b6061de321 (`uname -v` #1 allm2, one-shot, never promoted): lane/lapallm2 on main 6eb199f9, INTEL=1, .config
`CONFIG_MODULES=y` + `=m` for I915 HDA IWLWIFI E1000E USB_HID USB_STORAGE BT NVME USB_VIDEO_CLASS. Laptop left on good.

## A. YouTube NOPLAY: HARNESS, not the modules
Same fresh-boot procedure (reboot, `desktop-login.sh`, `dp.sh launch`, Big Buck Bunny) on the GOOD kernel also printed NOPLAY.
Causes: (1) /home is persistent now (lane/homevol), so Firefox starts with a fresh profile that has no `user.js`
(media.autoplay.default=0 / blocking_policy=0), and YouTube waits for a click. Fix: `dp.sh prefs` writes it (new, committed).
(2) `dp.sh waitplay` keys on i915 `exec_vcs`; with the fresh profile Firefox decodes in software (exec_vcs stays 0), so waitplay
reports NOPLAY even while the video plays. Criterion used here: pipewire sink running + RDD busy.
Evidence (pw-top, 3 samples, then RDD utime+stime ticks over 20 s):

| kernel | pw sink | Firefox stream | RDD ticks / 20 s |
|---|---|---|---|
| good daf77dba3e39 | leandros_output R 1024 quant 48000, BUSY 70-156 us | R 3600 F32LE 2ch 48000 (BUSY 22-62 us) | 8172+398 -> 9302+464 = 1196 (60 % of a core) |
| allm2 11b6061de321 | leandros_output R, BUSY 116-187 us | R 3600 F32LE 2ch 48000 (BUSY 39-78 us) | 8166+385 -> 9349+452 = 1250 |

Both play with sound; the all-modules kernel is indistinguishable from good. Hardware decode (exec_vcs) is not exercised by a fresh
profile on either kernel; the old deskplay2 numbers (exec_vcs 60/s) came from a profile with the HW-decode prefs.

## B. BT not autoloaded: REAL BUG, FIXED
`drivers/usb/src/xhci/mod.rs` enumerate: the Bluetooth branch (`btusb::scan_bt` hit) returns early with the function as
class 0xE0, BEFORE the generic `class::wants_or_load(...)` call that runs the kmod autoload hook. So with CONFIG_BT=m the
`usb * * e0 01 01` alias was never consulted for a real controller (the synthetic `usb.alias_selftest=bt` goes straight to
`wants_or_load`, which is why kmodbt passed). `adopt` then offered the function to an empty class registry and dropped it
(`NoMatch(_) if is_bt`). Fix: under `feature = "bt-module"` call `wants_or_load` in the BT branch before returning, so the
module loads and registers its driver and `adopt` offers it the function.
Proof on the laptop (Intel 8087:0a2b): klog `[kmod] usb 8087:0a2b e00101 -> bt`, `bt loaded`, `[btusb] controller 8087:0a2b on
slot 1`, `/dev/hci0 ready`, `[btd] ready bdaddr=... hci_ver=8`; `btctl show`: Controller ... State: ready; /proc/modules lists
bt Live with uvc usb_storage nvme i915 hda e1000e iwlwifi (usb_hid still unloaded: no HID device attached).
Before: lapallm trial had `btd is not running`.
