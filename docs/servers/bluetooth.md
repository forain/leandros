# Bluetooth host seam: /dev/hci0 and btd.sock

Lane btcore, 2026-10-26, updated 2026-10-30. Binding for the kernel HCI transports (Pi 5 UART, laptop
USB `btusb`) and for profile daemons (`bthid`; A2DP is not built). Code: `lib/bt-host` (sans-IO core, host tests:
`cargo test -p bt-host --target aarch64-apple-darwin`), `userland/btd` (`btd`, `btctl`, `bttest`,
and the `btd::client` library).

Starting it: init starts and supervises `btd`, `bthid` and `netctl watch` (see [Supervision](#supervision));
by hand, `btd &` as root once `/dev/hci0` exists. Without a controller btd prints
`[btd] no controller: /dev/hci0: errno N` and exits 1. Test hooks: `btd --fd N` (controller on an
inherited fd, any byte stream), `--sock PATH`, `--keys PATH`. Other flags: `--idle-ms N` (idle ACL
disconnect, default 5 s), `--grace-ms N` (grace for incoming links from bonded devices, default 20 s),
`--accept-peripheral` (see [Incoming links](#g-incoming-links-page-crossing-and-reconnect)).
`btctl show|scan [SECS]|devices|list|paired-devices|pair ADDR|connect ADDR [PSM]|disconnect ADDR|remove ADDR`
is the command-line client.

Layering:

```
 bthid / btctl / (later a2dp)        userland clients
        |  AF_UNIX stream  /run/bluetooth/btd.sock   (section B)
 btd  (userland/btd, links lib/bt-host)   HCI core, ACL, L2CAP, SSP, link keys
        |  read/write whole H4 packets   /dev/hci0    (section A)
 kernel (Pi 5: uarta 16550 + BT_ON + patchram + H4 framer; laptop: btusb + Intel firmware)
```

## A. Kernel raw HCI node `/dev/hci0` (Pi UART and btusb implement; btd consumes)

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
| 0x86 | LINK           | bdaddr[6] up:u8 incoming:u8 (BR/EDR ACL up/down; sent to LISTEN owners) |
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
- An ACL link with no channels and no pending op is disconnected after 5 s (`--idle-ms`); the rule does not
  fire on security traffic, and an incoming link from a bonded device gets a 20 s grace (`--grace-ms`).
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

## D. Pi 5 transport: UART HCI bring-up (`drivers/src/bt/`)

The Pi 5's CYW43455 Bluetooth is on `uarta` (`0x10_7d50_c000`). The device tree calls it
`brcm,bcm7271-uart`, so it is an 8250-family 16550 (register shift 2, 96 MHz clock, divisor 52 at 115200
and 2 at 3 Mbaud, `MCR.AFE` auto RTS/CTS), **not a PL011**; RX is polled because auto flow control holds the
sender off. `bt::bringup` follows Linux `btbcm`/`hci_bcm` over a `Transport` trait: HCI Reset, Read Local
Version, the BCM4345C0 minidriver and `.hcd` patchram (`BCM4345C0.hcd`, from Alpine's `linux-firmware-brcm` via `ports/alpine/firmware.list`;
each record is validated, exactly one Launch RAM and last), 250 ms, Reset, `0xfc18` to switch to 3 Mbaud,
then Read Version and Read BD_ADDR. The node `/dev/hci0` appears only after this succeeds. Knobs: `bt=0`
skips Bluetooth, `bt.baud=N` picks the operational rate, `bt.fw=` the patch file.

The bring-up took five rounds on real hardware, each a missing detail from the Linux driver:

1. **Pin mux.** The D0 firmware leaves GPIO 24-27 as plain GPIOs; with `MCR.AFE` the CTS input was
   unconnected, so the first Reset never left the FIFO (`Tx(3075)`). GPIO 24-27 are muxed to `uart0` (fsel 4,
   RTS/TXD with no bias, CTS/RXD pull-up) and `BT_ON` (GPIO 29) is driven low before a 50/200 ms power cycle
   (`pibt-mux`).
2. **Reset settle.** `btbcm_reset()` sleeps 100 ms after every HCI Reset; the controller drops commands in
   that window. `RESET_SETTLE_MS` = 100 after the ROM Reset and after the patched one, and the first Read Local
   Version is retried once (`pibt-ver`).
3. **Flow-control fallback.** If Read Local Version goes unanswered under auto flow control, the driver falls
   back to driver-driven RTS/CTS and logs the pad levels (`pibt-rts`).
4. **RTS at the `BT_ON` edge.** The BCM43xx samples CTS_N (the host's RTS) when `BT_ON` rises; with RTS
   asserted it answers Reset and then ignores commands. As Linux does for `brcm,bcm43438-bt`
   (`drive_rts_on_open`), RTS is held deasserted across the edge, and there is one cold power-cycle retry
   (`pibt-silent`).
5. **Address.** A placeholder BD_ADDR (`43:45:c0:00:1f:ac`) is replaced with `0xfc01`. `bt.bdaddr=auto|dt|otp|keep|xx:xx:xx:xx:xx:xx`
   chooses the source; on the Pi the OTP tag fails and the fallback is the firmware's Ethernet MAC (`smsc95xx.macaddr`) + 2. A new
   address invalidates bonds, so a pad must be re-paired once (or use `bt.bdaddr=keep`). Link keys are stored per
   address, which is why this matters.

`hcidev_core.rs` is the pure `/dev/hci0` machinery (write validation, RX queue of 64 packets dropping the newest,
TX queue of 16 answering `EAGAIN`, one-packet reads, `EMSGSIZE`, poll masks); `hcidev.rs` is the kernel handler
with an exclusive open. It is shared by the laptop transport, so it is no longer Pi-only.

## E. Laptop transport: `btusb` and the Intel 8265

On the HP EliteBook 840 G5 the Intel 8265 (`8087:0a2b`) is a USB function with class E0/01/01 (`lapbt`).
`drivers/usb/src/xhci/btusb.rs` opens the interrupt-IN and bulk IN/OUT endpoints with 16 one-packet TDs queued
per IN endpoint, sends commands on EP0 (class request `0x20`), and recovers a failed endpoint without ever
resetting the controller (the root is on USB). `bt::usb_reasm` reassembles H4 packets from USB chunks.
`bt::btintel` runs the legacy secure-bootloader flow: Read Version, Boot Params, the RSA `.sfi` sent as
`0xfc09` fragments (584092 bytes in 2361 fragments, 1331 ms in the lane's log), Intel Reset with the boot
address, wait for the operational image, then the DDC file and the event mask. An already operational
controller (warm reboot, kexec) skips the download. The firmware set (`ibt-12-16.sfi`/`.ddc`) is fetched by
Alpine's `linux-firmware-intel` via `ports/alpine/firmware.list`. Knobs: `bt=0`, `btusb.fw=0`. The `btusb` thread is spawned only when a BT function
was enumerated, and it registers the same `/dev/hci0`.

Hardening from review (`xhcisafe`): a failed xHCI endpoint disables only its slot, the transport claims the
function once, resets reassembly state, and retries the Intel set-up.

**Kernel-module status.** Bluetooth is a tristate (`CONFIG_BT`, default `y`; shipped configs keep `y`).
With `CONFIG_BT=m` the USB transport moves into `modules/bt` (feature `mod-bt`, alias `usb * * e0 01 01`) using
`kapi_hci_*` and `kapi_usb_bt_*`; the HCI core, the Pi UART transport, `btd` and `bthid` stay built in. See
[kernel/modules.md](../kernel/modules.md). The module is exercised by `scripts/kmod/autoloadtest.sh <arch> bt`
with a synthetic interface, because QEMU has no USB Bluetooth controller.

## F. Input devices: `bthid` and `/dev/uinput`

`userland/bthid` is the HID host (`lane/bthid`). It is a btd client that listens on PSMs 0x11 and 0x13, runs its
own SDP client on PSM 1 (PnP record for vendor/product, HID descriptor list with continuation), opens HIDP
control and interrupt channels for outgoing connects and accepts incoming reconnects. It caches descriptors in
`/var/lib/bluetooth/hid-<ADDR>` and keeps `/var/lib/bluetooth/hid-devices`. A report-descriptor parser
maps devices to a gamepad, keyboard or mouse role, one evdev node each; gamepads are normalised like Linux
(`-32768..32767` sticks, `0..255` triggers, hat, positional `BTN_SOUTH..`). Quirk paths exist for DS4,
DualSense, Switch Pro and 8BitDo in Switch mode (`057e:2009`) and a generic path for 8BitDo D-input and Xbox One S
classic-BT firmware. CLI: `bthid connect ADDR | forget ADDR | list | selftest SECS` (the selftest feeds synthetic
DS4 reports through the real parser).

The evdev nodes are made through `/dev/uinput` (`servers/evdev/src/uinput.rs`, a Linux ABI subset: `SET_*BIT`,
`ABS_SETUP`, `DEV_SETUP`, `DEV_CREATE/DESTROY`, `GET_VERSION/SYSNAME`, plus multitouch slots), `root 0600`, with
created nodes `root:input 0660` like USB ones. MAME sees four stable joystick devices ("Gamepad 1..4") by
scanning `event2..15` once a second. Real-hardware status: an 8BitDo pad bonded and connected incoming on the
Pi; the descriptors in the host tests are reconstructions, not captures. Open: Xbox Wireless on new firmware is
BLE HID-over-GATT, which needs HOGP (ATT/GATT client and SMP) in btd; LE is scan-only today.

## G. Incoming links, page crossing and reconnect

A sleeping pad wakes and pages the host rather than waiting to be paged, and both sides can page at once.
The lanes `pibtreconn`, `pibtacl` and `pibtrole` made this reliable enough to reconnect without a shell:

- **Page crossing.** An outgoing connect that loses to an incoming one (status `0x0b`, connection already
  exists) is not fatal; it waits up to 10 s for the incoming link, then reports busy. btd emits event `0x86 LINK`
  (`bdaddr`, `up`, `incoming`) to LISTEN owners, so `bthid` retries a BUSY once after 2 s and falls back to a
  connect after 3 s of silence from a LINK. A failed outgoing connect never tears down an existing session,
  duplicate PSM channels are closed, and incoming channels from unlisted addresses are closed.
- **Security on incoming links.** A bonded device's incoming link gets `start_security` and the 20 s grace; a
  non-SDP L2CAP request on an unencrypted link answers "pending, authentication" (channel state `WaitSecurity`)
  and completes on encryption, or is refused with a security block. An `IO_CAP_REQUEST` with no PAIR or outgoing
  channel is refused.
- **Role.** `ACCEPT_CONN` now accepts as **central** (role `0x00`) by default; `--accept-peripheral`,
  `BTD_ACCEPT_PERIPHERAL=1` or `/etc/btd.accept-peripheral` select the old peripheral role.
- **ACL robustness.** A new `CONN_COMPLETE` drops an older Up link for the same address (stale link), and a
  channel timeout with `acl_avail == 0` and no outstanding credits logs `ACL credit leak, restoring N` and
  resets the pool. `BTD_TRACE=1` or `/etc/btd.trace` (or `--trace`) logs `acl<`/`acl>`/`ncp`/`sig` lines, frames
  capped at 32 bytes of hex.

Status: the lane note recorded the reconnect as unverified (the pad was asleep, outbound page timeout `0x04`);
it was later verified end to end: on 2026-10-28, on the Pi 5 good kernel `f51a46029ffb`, the 8BitDo Lite 2 gamepad
auto-reconnected (user-confirmed). Incoming connection under role `0x00` showed `event2` and PSMs 0x11/0x13 open.

## Supervision

`lane/pibtauto`: init's stray sweep (run when greetd gives up for lack of a GPU) used to SIGKILL `btd`, `bthid`
and `netctl watch --stamp`. They are now entries of init's `DAEMONS` table, exempt from the sweep and respawned
from the idle tick with the sshd backoff (1, 2, 4, 8, 16 s; give up after five exits within 60 s; exit status 127
= no respawn). `btd` is gated on `/dev/hci0` and `bt=0`; `bthid` starts early, without `/dev/hci0` (`bthid=0` or
`bt=0` disable it).

## Applet build and permissions

`lane/cosmicbt` builds the unmodified COSMIC Bluetooth applet (`ports/cosmic-applet-bluetooth`, bluer 0.17.4,
linked against a client-only `libdbus-1.so.3` from `ports/libdbus`) and stages it with its `.desktop`, so it appears on the panel. The
`bluetooth` group (gid 112) owns `btd.sock` and `bthid.connect` (`root:bluetooth 0660`); `/run` is a tmpfs.
`leandros-sysbus` embeds its subcommand list so a stale shared binary is detected. The live applet-in-session
check was still open in the lane note.
