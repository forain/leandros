# lane/pibt-hci0 2026-10-27 — Pi 5 `/dev/hci0` raw HCI node (BT_SEAM section A)

Status: DELIVERED (host-tested; unproven on hardware until the next Pi boot). Seam unchanged.

## What
- `drivers/src/bt/hcidev_core.rs` (pure): `validate_h4_write` (01/03: 4-byte hdr, len at [3]; 02: 5-byte hdr,
  LE16 len at [3..5]; length must equal the rest, else EINVAL), `HciQueues` (RX 64 drop-newest + counter,
  TX 16 → EAGAIN, read = one whole packet / EMSGSIZE keeps it / EAGAIN / EIO when dead and empty,
  poll mask POLLIN/POLLOUT/POLLHUP|POLLERR, open/close flush; packets arriving with no open are discarded),
  `pump_once` (drain TX → `Transport::send`, then recv up to 1 ms for the first byte + whatever is pending,
  H4Parser → `[kind]+data` into RX; lock never held across UART I/O).
- `drivers/src/bt/hcidev.rs` (rpi5): kernel-handler port (OPEN exclusive by open_id CAS → EBUSY,
  READ/WRITE/POLL/CLOSE), poll key `poll_key(poll_class::BT = 25, 0)`, pump loop (wake on change,
  `[bt] rx drop n=` at most once per second, dead → one log line, idle).
- `boot::start(cmdline, serror, on_ready: fn(port))`: after `bring_up` Ok the bt thread creates the port,
  calls `on_ready` (kernel/src/init.rs registers `/dev/hci0`, 10:137 via `register_device_notify_open`),
  logs `[bt] /dev/hci0 ready` and becomes the pump. vfs `dyn_dev_meta`: (10,137) → root:root 0600.

## Evidence
- drivers/bt-tests 23/23 (8 new, fake H4 controller answering every command with Command Complete):
  HCI Reset `01 03 0c 00` → `04 0e 04 01 03 0c 00` read whole; poll POLLOUT → POLLIN|POLLOUT → POLLOUT.
  Mutation check (newest→oldest drop, EMSGSIZE off by one, queue while closed): 3 tests fail.
- check-rpi5.sh rpi5 + raspi4b clean (no warnings in bt/). aarch64 box regress 49 suites, 0 bad (d9e4fa75); Mac HVF vfstest PASS.

## Open
- No QEMU path for the UART; the blocking-read/epoll path through the VFS is exercised only on the Pi
  (expected: `[bt] hci ver=…` then `[bt] /dev/hci0 ready`; btd opens it).
- TX latency is up to 1 ms (the pump polls); IRQ-driven RX (SPI 276) would remove it.
