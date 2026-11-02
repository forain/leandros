# lane/kmodpins, 2026-11-01

## Pins (all evaluated by registry::busy_reason, before the Upper gate closes and again after close_and_drain(Upper), D9)
- Block fd: vfs_server::block_fd_open(path) scans every process fd table for VnodeKind::BlockDev of the disk or any partition (family_paths). Covers userspace fs servers/tools holding the node. Reason "block device open".
- Mount: unchanged (mounted / mounting). Only f2fs registers mounts; a userspace fs server pins through its block fd.
- V4L2: only /dev/video0 exists (single-device layer); open_by now requires DEV.owner != 0 and the camera's owner. The open claims DEV inside the Upper gate (GOps::in_gate).
- A refused unload reopens the gate, state Live; test reads the disk afterwards.

## Tests (scripts/kmod/unloadtest.sh UPPER=1)
FDBUSY_RC=1, "busy: block device open", FDREAD_RC=0, FDCLOSE_RM_RC=0, open-fd rmmod loop x20 (no success while open, no Zombie), FDLOOP_RM_RC=0.

## Gaps
- Partition fd not exercised (the test stick has no partition table); same code path via family_paths.
- uvc pin still unexercised (no UVC camera in QEMU).
- fd opened between the post-drain check and removal is not pinned (I/O fails -ENODEV-style, no freed-code access).
- Loop devices bound to a file on a module disk are not pinned.
