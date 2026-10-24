# lane/evdyn — 2026-10-24 (L8.2 remainder: per-device evdev nodes)

Status: FIXED in QEMU (aarch64). Every USB HID interface gets its own /dev/input/eventN. The node goes away on unplug, open readers get ENODEV and POLLHUP|POLLERR, and a re-plugged device gets a fresh node.

## Design
- servers/evdev: MAX_DEVICES 4 -> 16.
  - event0 (aggregate keyboard: virtio/PS2/serial console plus every USB keyboard) and event1 (pointer: virtio-tablet plus rel_pointer) are fixed and keep their numbers. libinput/COSMIC config and the tests are unchanged.
  - `register_input_device(kind, name, bus, vendor, product)` takes the lowest free event2..15. A number is free only when its old device is gone and no queue of it is still open, so a number is reused only after release (as on Linux).
  - `unregister_input_device(n)`: the node leaves the VFS registry (new `vfs_server::unregister_device`), so opens get ENOENT and the node leaves `ls`. Every open queue is revoked and woken. read/ioctl/write then return ENODEV and poll returns 0x18, and new queues on a gone node are refused. The VFS_CLOSE / release_pid paths free them.
  - Dynamic nodes register with notify_open, so the queue exists from open(2) on (no lazily-created queue on a reused number).
  - Per-device capabilities:
    - keyboard: EV_SYN/KEY/LED, KEY 1..255, LED num/caps/scroll.
    - mouse: EV_SYN/KEY/REL, BTN_LEFT..BTN_EXTRA, REL_X/Y/WHEEL.
    - EVIOCGNAME is the USB product string, and EVIOCGID is BUS_USB plus the device VID/PID.
    - EVIOCGKEY comes from the per-device key bitmap (evgkey).
  - VFS MAX_DYNAMIC_DEVICES 16 -> 32.
  - pidtrust is unchanged: the port stays kernel-only via `register_kernel_handler`, and registration is a kernel function call, not IPC.
- drivers/usb hid: `NodeHooks { attach, detach }` are installed by drivers/src/usb_hid.rs.
  - attach runs per interface at enumeration (task context).
  - detach runs from the hot-plug scan (task context). It also covers a device whose failure the IRQ-context poll saw first.
  - `poll` now passes the node id with each event.
  - The glue pushes keys to event0 and to the device node. Mouse reports go to event1 (absolute, as before) and as raw REL events to the device node.
- Hot-plug and COSMIC: libinput here enumerates through the libudev shim (ports/input-stack/shims/libudev), which has a static event0/event1 table, and its monitor fd never fires. No netlink/uevent or inotify source exists. A running compositor therefore never sees eventN>=2 appear or vanish. That is why USB input is still mirrored into event0/event1: hot-plugged keyboards and mice keep working in COSMIC. Making the shim enumerate /dev/input and adding a uevent-like monitor would let the mirror go (the Linux-faithful end state). Not done here.

## Guard
- `usbunplugtest.py hid` new steps (e)-(h):
  - kbd3 added -> a new /dev/input/eventN appears with name "QEMU USB Keyboard", bus=3.
  - Keys arrive on that node and are mirrored on event0.
  - An `evtest2 gone` reader gets `GONE-READ errno=19 revents=0x18` after device_del.
  - The node is absent from `ls`, and open gives ENOENT.
  - The remaining keyboard still delivers.
- New `evtest2 info|gone` subcommands.
- Results:
  - Red on main (1b776760, rebuilt, same script and evtest2): `FAIL: (e) no new eventN appeared for kbd3 within 10 s`.
  - Lane: all of (e)-(h) PASS, the hid phase PASSES, and kbd3 got /dev/input/event5.
- Full aarch64 regress (evgkeytest, evherdtest and the evtest suites included): all green except sshsmoke, a known flake that passed on re-run.
- COSMIC probe: clickmovetest under --virgl (VNC 5987). Greeter login by keyboard worked, and two cosmic-terms opened by the probe rendered (screenshot 3-term2.png).
- x86_64 release build: OK (PS/2 path untouched).

## Open
- libudev shim plus a hot-plug monitor (above), after which the event0/event1 mirroring can go.
- Real-hardware check on the laptop/PC (USB keyboard + mouse each showing their own node).
- In QEMU, injected keys reach a just-enumerated usb-kbd only after it has been polled for a while. The guard waits 1.5 s before step (f).
