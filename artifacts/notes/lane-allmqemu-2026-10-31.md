# lane/allmqemu 2026-10-31 (kernel-modules wave 5.9)

All tristates that apply to the QEMU boards built =m (VIRTIO_GPU stays bool; V3D/MACB/HDMI_AUDIO/SDHCI are Pi-only and were left out):
KMOD_HELLO VIRTIO_BLK VIRTIO_NET VIRTIO_KEYBOARD VIRTIO_RNG VIRTIO_SND E1000E IWLWIFI I915 HDA NVME USB_VIDEO_CLASS USB_HID USB_STORAGE BT, plus MODULES=y.
Applied as an untracked .config layered over the defconfig by build-all (15 .kmod files in the initrd on both arches).

Results (runtests.py <arch> allm-<arch> --suite regress):
- root is /dev/vdb f2fs, mounted through the virtio_blk module; /proc/modules shows virtio_blk, usb_storage, virtio_keyboard, virtio_snd, virtio_net, virtio_rng as Live
  (the rest have no device in the default QEMU machine, so alias autoload never fires).
- aarch64 and x86_64: every command passes except host:wifisimtest.py (4 sub-fails: proc_net_wifi, wifi0_dhcp, netctl_connect_ssid_bssid, wifisim_selftest).
- That failure is by design, not a bug: WIFI_SIM `depends on VIRTIO_NET=y`, so with VIRTIO_NET=m the simulated Wi-Fi device is not built.
  Keep VIRTIO_NET=y when running the wifisim/net-e2e suites.
- The i915 ddb exceptions did not show up (no failing i915 tests in the regress suite).
No code fixes were needed.
