# lane/lapallm - all laptop drivers as modules on the real laptop - 2026-10-30/31

PARTIAL. Build: main f80bc8ba, INTEL=1, .config = laptop defconfig (CONFIG_BOARD_QEMU=y) + MODULES=y + =m for I915 HDA IWLWIFI
E1000E USB_HID USB_STORAGE BT NVME USB_VIDEO_CLASS, tag `allm`. bidcheck ok; initrd carries all 9 .kmod + modules.alias; modules.load = nvme, usb_storage.
Trial c183ea294d87 (`uname -v` #1 allm), one-shot, never promoted; laptop back on good daf77dba3e39.

| item | result | evidence |
|---|---|---|
| 1 trial + modules + stick root | PARTIAL | booted trial c183ea294d87 #1 allm; /proc/modules Live: uvc usb_storage nvme i915 hda e1000e iwlwifi; usb_hid and bt NOT loaded (no matching device event in klog) |
| 2 keyboard evdev | NOT PROVEN | /dev/input event0-3 exist; i8042 kbd probe ok; usb_hid never loaded (no USB HID device attached); guest has no grep so devices list not captured |
| 3 Wi-Fi WPA2 | PASS | klog: `iwlwifi associated AID 5`, `DHCP configured, address: 172.16.149.179 (wifi0)`; SSH ran over wifi0 |
| 4 COSMIC on i915 | PASS | desktop-login: session up ~6 s; gpuprobe gl: Mesa Intel UHD Graphics 620 (KBL GT2) / OpenGL ES 3.2 Mesa 25.3.6; i915 gt exec rcs ~38/s |
| 5 YouTube + sound | FAIL (not module-related) | dp.sh waitplay: NOPLAY after 120 s; matches the open P1 (lane-deskplay: Firefox over SSH in a greetd-login session never plays); sink counters not captured (no grep in guest) |
| 6 BT | FAIL | `btctl show`: "btd is not running"; no bt module load in the trial klog; an earlier boot's klog-all shows btusb hci0 + btd ready (built-in path) |

Findings: (a) usb_hid/bt =m have no alias hit in this boot, so those paths are untested on hardware; to test usb_hid plug a USB keyboard; bt
needs the 8087:0a2b alias to fire (check the alias table / USB enumeration with bt=m). (b) The measurement script used grep, absent in the guest: use
the shell loops in scripts/deskplay. Rolled back: reboot -> good daf77dba3e39. Not run again for budget.
