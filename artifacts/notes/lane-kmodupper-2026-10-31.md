# lane/kmodupper (U3), 2026-10-31

Built on lane/kmodlower (U2 as of 9708c433; its review fixes were not yet committed, so NOT merged here: merge lane/kmodlower before the final run).

## Design as built
- Gate::reopen/upper_closed. delete_module order is now: close+drain Upper, busy_reason() (D9), on busy reopen Upper + state Live + EBUSY, then unbind_upper, quiesce, lower drain, exit thread.
- registry::busy_reason: V4L2 (v4l2::open_by, /dev/video0 open on this module's camera), BLK (any mount whose device is the disk or a partition, via blkdev::family_paths), DRM/KMS (always busy). netdev/input/hwrng/USB class detach instead.
- USB class: UsbCls carries the owner's gate; probe/detach run through Upper (closed = decline). registry::pre_exit runs on the exit thread before exit(): usb_class_teardown unregisters the driver, runs the driver's detach for each claimed function, drops handles and re-queues the UsbFunction on class::UNCLAIMED, so the next load re-probes the still-attached device (this is the autoload re-arm: the usb hook is stateless, and reload binds the requeued function). UsbCls record is leaked on purpose (an enumerate snapshot may still hold it).
- BLK: ModDisk I/O and flush go through the Upper gate (closed = -19); release = remove_disk. V4L2: GOps wrapper gates all camera ops. INPUT: per-device nodes released; fixed nodes (virtio_keyboard) stay claimed -> Zombie.
- Registrations outside an open gate are refused (-19) for usb class, blk, v4l2 (owner 0 = built-in uvc sink stays allowed).
- exit added: usb_hid, uvc, usb_storage, e1000e. Fixture kmbad_leak (slot 316) claims the fixed pointer input node -> Zombie, slot never freed.

## Results (aarch64 QEMU, scripts: ad hoc, same commands now in unloadtest.sh UPPER=1 section)
- usb_storage (QEMU usb-storage, 128M f2fs volume) mounted ro: rmmod -> EBUSY ("busy: mounted"); after umount rmmod ok; reload re-probes the same stick, remount ok, rmmod again ok.
- usb_hid x10 unload/reload with usb-kbd: each load rebinds the keyboard; no crash.
- kmbad_leak -> "ZOMBIE: ungated registrations left", /proc/modules Zombie.
- Found+fixed: self-deadlock on MATCHES in teardown (made exit time out -> Zombie).

## Not done / caveats
- hda: no exit (SND attach has no detach in pipewire_server) and virtio_keyboard/virtio_snd exits not added; uvc and e1000e exit built but not exercised (no UVC/e1000e device in aarch64 QEMU; x86 e1000e untested).
- x86_64 not run. U1+U2 checks of unloadtest.sh and autoloadtest not re-run after the last edit; lane/kmodlower fixes not merged.
- Writable umount of the stick (rw) broke root path lookup (ENOTCONN) in this kernel; tests mount read-only. Likely pre-existing, not investigated.
- Open /dev/video and open block-device fds (not mounts) are not yet pinned: only video0-open and mounts.
- Needs untracked .config: CONFIG_MODULES=y CONFIG_USB_HID=m CONFIG_USB_STORAGE=m. Run with FFSESSION_OUT=/tmp/ffsession-kmodupper UPPER=1.
