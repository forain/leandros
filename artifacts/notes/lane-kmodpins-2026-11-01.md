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
- fd opened between the post-drain check and removal is not pinned (I/O fails EIO (-5; ENXIO -6 once the slot is retired), no freed-code access).
- Loop devices bound to a file on a module disk are not pinned.

## Review fixes (2026-11-01)
- block_fd_open replaced by vfs_server::block_family_busy(paths): family = BDEVS slots by index (disk + every Backing::Part with that parent, so BLKPG partitions count), one pass; fd scan under scan_gate(); SCM_RIGHTS in-flight fds counted via BLK_INFLIGHT (inc/dec beside tmp_inflight_inc/dec); mount check by family names; loop bound directly to a family member -> "loop device bound".
- errno for an escaped fd is EIO (-5), ENXIO (-6) after retire; an fd opened between pre-check and post-drain check sees transient EIO while the gate is closed even if the unload is refused.
- unloadtest refuses (exit 2) when a needed module is not =m in .config. Loop pin test runs only if /bin/losetup exists in the guest; no BLKPG tool in the guest, so the BLKPG partition pin is not exercised.
- Test result (aarch64, .config from kmodexit, UPPER=1): 41 ok, 2 FAIL (kmbad_leak Zombie x2): with VIRTIO_KEYBOARD=m autoloaded, kmbad_leak's input_register gets EBUSY (-16) because virtio_keyboard holds the keyboard slot and this base cannot rmmod it (no exit()); resolved by lane/kmodexit (virtio_keyboard exit), re-run after merge. The first run had 3 net FAILs (eth1 / DHCP) that did not recur in later runs (flaky, unexplained).
- No losetup / BLKPG tool in the guest: loop-on-device and BLKPG partition pins have no unloadtest case.
