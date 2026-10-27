# lane/btcore 2026-10-27: Bluetooth host core (HCI, L2CAP, SSP, btd, btctl)

**DELIVERED** (userspace half; the kernel `/dev/hci0` is lane pibt-uart's).

## Where it lives, and why
- `lib/bt-host`: a sans-IO, `no_std + alloc` state machine (HCI, ACL, L2CAP, pairing, key file
  format, client wire format). It does no I/O, so its whole behaviour is host-tested with exact
  HCI bytes. It is reusable by a future btusb or in-kernel user.
- `userland/btd`: the daemon that owns `/dev/hci0`, and the `btd::client` library that profiles
  link. Note that `servers/net` and `servers/tty` are in-kernel crates. Bluetooth is a userspace
  daemon, like BlueZ's bluetoothd, for four reasons: the kernel seam stays tiny (raw H4), pairing
  and policy bugs can't panic the kernel, link keys live on disk, and profiles are plain processes.
- Seam: `docs/servers/bluetooth.md` (same as the scratchpad `BT_SEAM.md`). It covers (A) the
  `/dev/hci0` contract (one whole H4 packet per read and write, exclusive open, post-patchram)
  and (B) the btd.sock frame protocol and `btd::client::Bt` (`connect/listen/send/next/close`,
  plus `pair/remove/devices/info`).

## Scope done
- Init: reset, version, features, bdaddr, buffer sizes (+LE), event mask, SSP mode, EIR
  inquiry mode, LE host, LE event mask, class of device (computer/laptop), name, page timeout,
  scan enable (page+inquiry). The init is fatal only if reset, bdaddr or buffer size fails.
- Discovery: inquiry results (plain, RSSI, EIR names), then remote-name requests for nameless
  devices, plus an LE active scan (AD names, random-address flag), all into one device list.
- ACL: create/accept connections, credit flow control (Number_Of_Completed_Packets, credits
  returned on disconnect), fragmentation and reassembly. Idle links are dropped after 5 s
  without channels.
- L2CAP: connection, configuration (MTU, basic mode only, unknown-option handling), disconnect,
  echo, info (feature mask / fixed channels), command reject. Incoming channels are
  auto-accepted on listened PSMs, and other PSMs get "PSM not supported".
- Security: every non-SDP PSM needs authentication and encryption first. SSP uses
  NoInputNoOutput + general bonding (just-works; numeric comparison auto-accepted), passkey
  entry gets a negative reply, legacy PIN is `0000`, and a stale key (0x06) is deleted and
  re-paired once. Keys go to `/var/lib/bluetooth/linkkeys` atomically (tmp + fsync + rename),
  **before** the PAIR result reaches the client.
- LE fixed channels: ATT (MTU 23; requests get "not supported"; indications confirmed), SMP
  (pairing not supported), LE signalling (connection parameter update rejected).
- `btctl show|scan|devices|list|paired-devices|pair|connect ADDR [PSM]|disconnect|remove`.
  `connect ADDR 0x13` hex-dumps HID interrupt reports, which is useful for gamepad bring-up.

## Evidence
- `cargo test -p bt-host --target aarch64-apple-darwin`: 11/11. The tests are:
  - byte-exact traces for inquiry/EIR/RSSI/remote name, and for connection complete → SSP IO-cap
    exchange → encryption → L2CAP connect/config/data/disconnect;
  - an incoming channel plus a refused PSM;
  - PIN, passkey and page timeout;
  - an LE advertising report plus the ATT/SMP stubs;
  - frame and key-file round trips;
  - a loopback virtual controller: pair, connect, a 150-byte echo split into 6 ACL fragments at
    ACL MTU 27, close, idle disconnect, then a restart with the keys loaded from the file
    (LINK_KEY_REPLY path, no SSP), and stale-key re-pairing.
  A mutation check (wrong IO capability) fails 3 of the 10 tests.
- In guest (aarch64 HVF): `/bin/bttest` passes 8/8 in 1.7 s. It runs the real `/bin/btd` on a
  socketpair against a scripted BCM4345C0 (lmp_subver 0x6119) and covers:
  no-controller exit 1, ready, info, scan, SSP pair with the key on disk, the device list
  (paired+connected), a bad PSM, and remove.
  `bttest` is now part of the default regress suite.
- Full aarch64 regress on the box (TCG, b2925820): 49/50 green. The one red was the new
  bttest `btd_ready`, a **real bug** that the faster HVF run on the Mac hid. `Host::new` stamped
  the initial HCI_Reset at clock 0, so the first `tick(uptime)` timed it out ("command 0x0c03
  timed out", controller declared dead). It is fixed by `Host::new(.., now)`, with a guard test
  that is red on the unfixed code and green with the fix (11/11). The box rerun of `/bin/bttest`
  on aarch64 TCG then passed.

## Open / next
- Not started by init: start `btd &` once pibt-uart's `/dev/hci0` exists, then add it to init
  (respawn) after the first Pi boot.
- btd.sock is root-only (0600). It needs a `bluetooth` group before non-root `btctl` can use it.
- Client writes are blocking; a client that never reads can stall btd. Fix with per-client
  queues when profiles multiply.
- Keyboards that demand MITM (passkey) are refused because we are NoInputNoOutput. A display
  passkey path (KeyboardDisplay + print the code) is needed for user priority 3.
- No SDP server/client. HID hosts don't need it to connect, but the descriptor normally comes
  from SDP, so bthid needs either an SDP client (`connect psm 1`, a raw L2CAP channel, works
  today) or per-device quirks.
- LE: scan only. Central connections, GATT and SMP pairing (HOGP gamepads such as Xbox
  Series pads in BLE mode) are later lanes.
- Real-hardware validation waits for pibt-uart (Pi 5).
