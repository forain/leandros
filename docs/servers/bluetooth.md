# Bluetooth host seam: /dev/hci0 and btd.sock

Lane btcore, 2026-10-26. Binding for the kernel HCI transport (pibt-uart, later btusb) and for
profile daemons (bthid, later A2DP). Code: `lib/bt-host` (sans-IO core, host tests:
`cargo test -p bt-host --target aarch64-apple-darwin`), `userland/btd` (`btd`, `btctl`, `bttest`,
and the `btd::client` library).

Starting it: `btd &` as root once `/dev/hci0` exists (not started by init yet). Without a
controller btd prints `[btd] no controller: /dev/hci0: errno N` and exits 1. Test hooks:
`btd --fd N` (controller on an inherited fd, any byte stream), `--sock PATH`, `--keys PATH`.

Layering:

```
 bthid / btctl / (later a2dp)        userland clients
        |  AF_UNIX stream  /run/bluetooth/btd.sock   (section B)
 btd  (userland/btd, links lib/bt-host)   HCI core, ACL, L2CAP, SSP, link keys
        |  read/write whole H4 packets   /dev/hci0    (section A)
 kernel (pibt-uart: 16550 + BT_ON + patchram + H4 framer; later btusb)
```

## A. Kernel raw HCI node `/dev/hci0` (pibt-uart implements; btd consumes)

- Char device, `root:root 0600` (only btd opens it). Exclusive: a second `open` fails `EBUSY`.
- The node exists only once the controller is **post vendor init**: patchram (`.hcd`), baud
  switch, BD_ADDR fix-up are the kernel's job. btd starts with its own `HCI_Reset` (BCM keeps the
  RAM patch across HCI_Reset; Linux btbcm also resets after download).
- `write(fd, buf, n)`: exactly **one whole H4 packet** per call, type byte first:
  `0x01` command (`01 op_lo op_hi plen params…`), `0x02` ACL (`02 h_lo h_hi l_lo l_hi data…`),
  `0x03` SCO. Returns `n`, `EINVAL` if the packet is short/inconsistent (length field != n-hdr),
  `EAGAIN` with `O_NONBLOCK` if the TX queue is full (POLLOUT clears/sets accordingly).
- `read(fd, buf, n)`: returns exactly **one whole H4 packet** (`0x04` event, `0x02` ACL, `0x03` SCO,
  `0x05` ISO), never a partial one, never two. If `n` is too small for the next packet: `EMSGSIZE`,
  packet stays queued. btd always reads with a 4096-byte buffer. `O_NONBLOCK`: `EAGAIN` when empty.
- `poll`: `POLLIN` when a packet is queued; `POLLOUT` when a write will not block; `POLLHUP`/`EIO`
  if the controller died (btd logs and exits).
- RX queue >= 64 packets; on overflow the kernel drops the **newest** and counts it (`[bt] rx drop`).
- No ioctls needed. Flow control (HCI command credits, ACL Number_Of_Completed_Packets) is btd's job.
- btd is also robust to a byte stream (it re-frames H4), so a pty/socketpair can stand in for tests.

## B. btd client API `/run/bluetooth/btd.sock` (btd implements; bthid/btctl consume)

AF_UNIX `SOCK_STREAM`, root:bluetooth 0660 (members of the `bluetooth` group, gid 112, Debian's convention; the
desktop's org.bluez shim runs as the session user, who is a member; others get EACCES). Every message is a frame: `len:u16 LE | op:u8 | payload` where `len` counts
`op + payload`. All integers little-endian. `bdaddr` = 6 bytes in **HCI order** (LSB first, i.e.
reversed from the printed `AA:BB:CC:DD:EE:FF`). Encode/decode is `bt_host::api` (lib/bt-host,
no_std, host-tested); the blocking client is `btd::client` (userland/btd, `[lib]`):

```rust
use btd::client::Bt;                       // dependency: btd = { path = "../btd" }
let mut bt = Bt::open()?;                  // connects to btd.sock
let ch = bt.connect(addr, 0x11)?;          // Channel { cid, peer_mtu }; blocks until open/fail
                                           // errors: client::Error::{Io(errno), Status(code)}
bt.listen(0x11)?; bt.listen(0x13)?;        // incoming channels on these PSMs go to THIS client
match bt.next()? {                         // blocking; or poll bt.fd() then bt.next()
    Msg::Recv { cid, data } => ..,        // one L2CAP SDU (reassembled)
    Msg::Incoming { cid, addr, psm, .. } => .., // already open (auto-accepted)
    Msg::Closed { cid, reason } => ..,
    _ => ..
}
bt.send(cid, &sdu)?; bt.close(cid)?;
```

Requests (client -> btd):

| op   | name    | payload                       | reply |
|------|---------|-------------------------------|-------|
| 0x01 | CONNECT | req:u16 bdaddr[6] psm:u16     | CONNECT_RESULT |
| 0x02 | LISTEN  | psm:u16                       | RESULT(op=0x02) |
| 0x03 | SEND    | cid:u16 data…                 | none (errors -> CLOSED) |
| 0x04 | CLOSE   | cid:u16                       | CLOSED |
| 0x10 | SCAN    | secs:u8 (0 = stop)            | DEVICE… then RESULT(op=0x10) at end |
| 0x11 | DEVICES | -                             | DEVICE… then RESULT(op=0x11) |
| 0x12 | PAIR    | bdaddr[6]                     | RESULT(op=0x12) |
| 0x13 | REMOVE  | bdaddr[6]                     | RESULT(op=0x13) |
| 0x14 | INFO    | -                             | INFO |
| 0x15 | DISCONNECT | bdaddr[6]                  | RESULT(op=0x15) |

Replies / events (btd -> client):

| op   | name           | payload |
|------|----------------|---------|
| 0x81 | CONNECT_RESULT | req:u16 status:u8 cid:u16 peer_mtu:u16 |
| 0x83 | RECV           | cid:u16 data… |
| 0x84 | CLOSED         | cid:u16 reason:u8 (status code) |
| 0x85 | INCOMING       | cid:u16 bdaddr[6] psm:u16 peer_mtu:u16 |
| 0x90 | DEVICE         | bdaddr[6] flags:u8 (1 paired, 2 connected, 4 LE, 8 LE-random) rssi:i8 class:u24 name… (UTF-8, rest of frame) |
| 0x91 | RESULT         | op:u8 status:u8 |
| 0x92 | INFO           | bdaddr[6] hci_ver:u8 lmp_subver:u16 manufacturer:u16 state:u8 (0 init, 1 ready, 2 dead) name… |

Status codes: 0 ok, 1 no controller/not ready, 2 page timeout (unreachable), 3 auth failed,
4 refused (PSM not supported), 5 security block, 6 no resources, 7 busy, 8 bad request,
9 timeout, 10 remote closed / link lost, 11 local close.

Semantics:
- CONNECT brings up the ACL link if needed (Create_Connection). For any PSM other than SDP (1) the
  link is first **authenticated + encrypted** (HID 0x11/0x13 require it): stored link key, else SSP
  (we are IO cap NoInputNoOutput -> just-works; numeric comparison auto-accepted), else legacy
  PIN `0000`. Then L2CAP basic mode, our MTU 672, peer MTU from its config request.
- Channels belong to the client socket that opened/accepted them; when it disconnects btd closes
  them and drops its LISTENs. A PSM can be listened by one client only (second -> status 7 busy).
- Incoming channels on a listened PSM are accepted and configured by btd; the client sees INCOMING
  only when the channel is open. Unlistened PSMs get "PSM not supported".
- SEND payload <= peer_mtu (larger SDUs are dropped with a log line). btd fragments into ACL.
  SEND on an unknown/foreign cid answers CLOSED(cid, 8).
- An ACL link with no channels and no pending op is disconnected after 5 s.
- Link keys: `/var/lib/bluetooth/linkkeys`, one line per device:
  `AA:BB:CC:DD:EE:FF <32 hex key> <key type> <name>`; written atomically (tmp + rename).
- LE: scan only (advertising reports -> DEVICE with flag 4). Incoming LE links get ATT (MTU 23,
  everything else "request not supported") and SMP ("pairing not supported") stubs. HOGP later.

## C. org.bluez for the desktop (`leandros-sysbus bluez`, ports/sysbus/src/bluez)

The COSMIC Bluetooth applet (bluer 0.17) and Settings > Bluetooth (bluez-zbus) talk BlueZ 5 D-Bus.
`leandros-sysbus bluez` (D-Bus activated from `org.bluez.service` on the session busd, which also
serves as the system bus) exports `/org/bluez` (AgentManager1, ProfileManager1), `/org/bluez/hci0`
(Adapter1, only while btd reports a ready controller) and `/org/bluez/hci0/dev_AA_BB_..` (Device1),
ObjectManager at `/`. StartDiscovery = btd SCAN in 30 s rounds (Discovering stays true) until
StopDiscovery (10 min cap); Pair = PAIR; RemoveDevice = REMOVE; Disconnect = DISCONNECT.
Connect on an input-class device (major class 5) appends `AA:BB:CC:DD:EE:FF` to bthid's spool
`/run/bluetooth/bthid.connect` (bthid creates it root:bluetooth 0660) and waits up to 15 s for the link; other
devices get an SDP channel held open until Disconnect. Connected/Paired come from btd DEVICES,
polled every 1.5 s. Powered is a soft switch (btd keeps the controller up). Agents are accepted
and never called (btd pairs just-works). Test: `dbus-run-session -- leandros-sysbus bluez-test`
(regress), which runs `bttest serve SOCK KEYS` (btd on the scripted controller).
