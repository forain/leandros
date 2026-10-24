# lane/usbunplug — 2026-10-23 (L8.2 remainder)

Status: FIXED (QEMU-testable parts); hub-attached devices and HID unplug stay open.

## What changed
- Background LUN readiness: the boot probe waits at most `PROBE_READY_MS` = 1 s for a LUN that is
  becoming ready (was 10 s per LUN, serially, with boot waiting). A device with no ready LUN goes to
  the `usb-hotplug` kthread, which retries `ScsiDisk::init` with one TEST UNIT READY per round
  (back-off 0.5 s doubling to 4 s) and registers the disk when ready. `ScsiDisk::init` now returns
  `NotReady` (instead of `NoDevice`) when a LUN was still spinning up.
- Fault knob `usbms.notready_ms=N`: every USB LUN answers NOT READY / 04 for N ms.
- Unplug: the `usb-hotplug` thread scans root-port CCS every 250 ms (`usb::hotplug_scan`).
  On disconnect of a mass-storage device: the `UsbDisk` is marked gone (requests fail with
  `BlkErr::NoDevice` before and after taking the disk lock), the request on the wire fails with
  `Transport(Disconnected)` (wait_td already watched CCS), the block entries are retired
  (`blkdev::remove_disk`: index kept, no name/I/O; ordinal of a re-plugged stick reused, so it is
  `sda` again), the VFS mirror drops the nodes (`removed_gen` op; retired slots are never reused,
  so an fd still open fails instead of reaching a new disk), then the xHCI slot is disabled and
  freed (`usb::release_port`). Device objects stay leaked for open fds.
- Re-plug: a newly connected root port is debounced (100 ms), reset and enumerated
  (`xhci::enumerate_hotplug`), and goes through the same probe path.
- `SYS_BLKDEV_NAME` returns ENODEV for a retired entry.

## Guard: `.claude/skills/run-leandros/usbunplugtest.py [arch] [unplug|notready|all]`
- unplug: two sticks; a read loop on A; QMP `device_del` A; expect the reader to fail within 6 s,
  A's node gone, shell RTT < 3 s, B still correct; `blockdev-add` + `device_add` A again; A's
  contents back on a `/dev/sd*` within 20 s, B still correct.
- notready: boot with `usbms.notready_ms=12000`; expect the "became ready ... in the background"
  line and the stick readable.
- Fix (aarch64): unplug PASS (reader rc=1 after 0.7 s, log `read failed: Transport(Disconnected)`,
  in-flight drained in 0 ms, 1 slot released, shell RTT 0.6 s, sdb intact, re-plugged stick back as
  /dev/sda in 1.0 s on port 3); notready PASS ("not ready after 1000 ms, polling in the background",
  "became ready after 15567 ms in the background", stick readable).
- Main 17e37fc0 image: RED. The reader also failed fast (0.8 s, wait_td already watched CCS), but
  /dev/sda stayed after unplug and the re-plugged stick never appeared (21.9 s); notready had no
  background line.
- aarch64 full regress: 45 suites 1/1 RC=0, overall PASS. x86_64 release build OK.
- An earlier run hit one "FAIL: boot" (driver start) on the two-stick setup; the next three boots of it were fine.

## Open
- Devices behind hubs are not hot-plug tracked (no hub support at all yet).
- HID unplug is not handled (entry kept, the pipe stops reporting); HID hot-plug is enumerated and
  registered but untested.
- A disconnect + reconnect faster than the 250 ms scan (CCS stays 1) is missed; PORTSC.CSC is not used.
- Real-PC check of a slow stick (the 10 s case) still wanted.
