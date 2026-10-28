# design-pibtreconn-2026-10-28: bonded HID pad reconnecting at boot (Pi 5, 8BitDo Lite 2)

Scope: lib/bt-host/src/host.rs (btd core), userland/btd/src/main.rs (knobs),
userland/bthid/src/main.rs. Line numbers are against main 32ef185a.

## Observed
bthid start -> CONNECT(E4:17:D8:82:A2:0A, 0x11) -> `connect ... failed status 0x0b` ->
`incoming connection from E4:..` -> `connected E4:.. handle 0x00b` -> 5 s -> `idle, disconnecting`
-> `disconnected reason 0x16`. No channel, no evdev node.

## Root causes (from the code)

### RC1 (high): btd has no page-crossing handling, so the boot race always ends with bthid's request dead
- `ensure_link` (host.rs:681-693) creates a Connecting link and sends Create_Connection.
- The pad was already paging us. The controller had a baseband ACL in setup, so it answered
  Create_Connection with 0x0b (ACL already exists). `cmd_done` (host.rs:284-290) treats every
  non-zero CREATE_CONN status as fatal: `link_down` -> bthid gets CONNECT_RESULT status 7 (BUSY,
  hci.rs:146).
- Then the pad's Connection_Request was accepted as a fresh link (host.rs:372-385). Note the other
  ordering is also broken: if Connection_Request had arrived while our Connecting link existed,
  host.rs:374 `!self.links.iter().any(|l| l.addr == a)` REJECTS it (0x0d), so the crossing is
  lost either way.
- Linux merges the two: hci_conn_request_evt reuses the existing hci_conn for the address,
  and a 0x0b/failed create on a conn that is being accepted is not fatal.

### RC2 (high): bthid gives up on a failed connect and never acts on the incoming ACL
- EV_CONNECT_RESULT failure (bthid main.rs:451-454) calls `teardown`, which clears `outgoing`
  and does not retry. bthid only learns of a device through its own CONNECT or EV_INCOMING
  (L2CAP). btd sends nothing when an ACL comes up, so bthid cannot fall back to
  "connect 0x11 over the ACL that already exists".
- Latent: the same `teardown` closes ctrl/intr cids, so a CONNECT failure arriving after the pad
  has opened its channels would kill the session that is forming.
- Latent: EV_INCOMING 0x11 while ctrl is already set (outgoing and incoming crossed) overwrites
  `ctrl` and leaks the older cid (main.rs:474-479).

### RC3 (high): btd's idle rule kills the incoming ACL before the pad's L2CAP shows up
- tick (host.rs:1225-1238): any BR/EDR link Up with no channel and no pair for 5 s
  (IDLE_DISCONNECT_MS, host.rs:30) is disconnected. It applies the same 5 s to a link the bonded
  device just made and that nobody has used yet. `last_activity` only moves on Conn Complete,
  our own AUTH_COMPLETE and ACL rx (host.rs:402, 424, 822). LINK_KEY_REQUEST, ENCRYPT_CHANGE and
  IO-cap events do not move it.

### RC4 (medium, why the pad itself opened nothing within 5 s): btd never secures an incoming link
- Incoming L2CAP (does it work?): yes, structurally. on_sig 0x02 (host.rs:934-956) accepts any
  listened PSM and auto-configures. bthid listens on 0x11/0x13 (main.rs:555-557) and handles
  EV_INCOMING ctrl then intr -> activate (main.rs:468-482). The incoming path is covered by the
  host test `incoming_channel_on_listened_psm_and_refused_psm` (tests.rs:344).
- Encryption on that path: it does not exist. btd only calls start_security from `advance()` for
  OUR WaitLink channels (host.rs:713-715). On an incoming ACL nothing asks for Authentication.
  The stored key is used only if the pad starts auth itself (LINK_KEY_REQUEST, host.rs:~475).
  Many HID devices (Apple, several 8BitDo firmwares) do not send the HID L2CAP Connection Request
  until the link is encrypted, because they expect the host (acceptor) to enforce security. This
  fits "ACL up, then 5 s of silence". It is not proven: the log has no line for
  LINK_KEY_REQUEST, ENCRYPT_CHANGE or signalling rx. The new log lines below settle it.
- Security hole on the same path, fix it here as well: incoming 0x11/0x13 is accepted on an
  unauthenticated, unencrypted link from ANY address, and IO_CAP_REQUEST (host.rs:~501) always
  answers just-works. Any nearby device can open HID channels and inject keys
  (CVE-2023-45866 class). bthid creates uinput nodes for any address (main.rs:468-482, activate).

## Design

### btd core (lib/bt-host/src/host.rs)

Link gets new fields (struct at host.rs:~70-85, init in new_link host.rs:663-670):
```rust
paging: bool,      // our Create_Connection is outstanding
accepting: bool,   // we sent Accept_Connection_Request
incoming: bool,    // the peer made this ACL (or won a crossing)
used: bool,        // some L2CAP channel was ever opened on it
up_at: u64,
expect_until: u64, // 0x0b seen: wait for the controller's incoming Conn Complete until then
```
ensure_link sets `paging = true`.

1. CONN_REQUEST (host.rs:372-385): adopt instead of reject.
```rust
ev::CONN_REQUEST => {
    need!(10);
    let a = Addr::from_slice(p);
    let existing = self.links.iter().position(|l| l.addr == a);
    let ok = p[9] == 1 && self.state == State::Ready
        && existing.map_or(true, |i| self.links[i].state == LinkState::Connecting);
    if ok {
        let i = match existing {
            Some(i) => { self.log(format!("incoming connection from {} crosses our page; adopting", a)); i }
            None => { self.log(format!("incoming connection from {}", a)); self.new_link(a, false, LinkState::Connecting) }
        };
        let l = &mut self.links[i];
        l.accepting = true; l.incoming = true; l.since = self.now;
        let mut c = a.0.to_vec(); c.push(0x01);
        self.send_cmd(op::ACCEPT_CONN, &c);
    } else { /* reject as today, reason 0x0d */ }
}
```
2. cmd_done CREATE_CONN failure (host.rs:284-290):
```rust
op::CREATE_CONN if st != 0 && params.len() >= 6 => {
    let a = Addr::from_slice(&params);
    if let Some(i) = self.links.iter().position(|l| l.addr == a && l.state == LinkState::Connecting) {
        self.links[i].paging = false;
        if self.links[i].accepting || st == 0x0b {
            if !self.links[i].accepting { self.links[i].expect_until = self.now + CROSS_WAIT_MS; }
            self.log(format!("connect {}: status 0x{:02x}, ACL exists, reusing the incoming link", a, st));
        } else {
            self.log(format!("connect {} failed status 0x{:02x}", a, st));
            self.link_down(i, hci::status_of(st));
        }
    }
}
```
3. CONN_COMPLETE failure (host.rs:388-393): if `p[0] == 0x0b`, or `paging && accepting` (two
   attempts in flight: this failure is one of them), clear `paging` and keep the link. Otherwise
   `link_down` as today. On success: set `paging = accepting = false`, `up_at = now`, then
   `advance(a)`. Also NEW: if `incoming && self.keys.get(a).is_some()` call `start_security(i)`
   (Windows/Android behaviour; satisfies devices that wait for encryption) and emit
   `Msg::Link { addr, up: true, incoming }` to every client with a LISTEN (see API below).
4. tick (host.rs:1218-1238):
   - Connecting with `!paging && !accepting && expect_until != 0 && now > expect_until` ->
     `link_down(i, BUSY)` and log `connect {a}: no incoming link after 0x0b`.
   - Idle limit per link:
```rust
let grace = l.incoming && !l.used && self.keys.get(a).is_some();
let limit = if grace { self.incoming_grace_ms } else { self.idle_ms };
let idle = l.state == LinkState::Up && !l.le && !l.auth_pending && !l.enc_pending
    && now.saturating_sub(l.last_activity.max(if grace { l.up_at } else { 0 })) > limit
    && !self.chans.iter().any(|c| c.addr == a) && !self.pairs.iter().any(|p| p.1 == a);
```
     Log `"{}: idle {} ms, disconnecting"` with the limit.
   - Set `last_activity = now` in LINK_KEY_REQUEST, ENCRYPT_CHANGE, IO_CAP_REQUEST/RESPONSE,
     USER_CONFIRM_REQUEST and on_sig. Set `used = true` when a channel reaches Open.
5. Incoming L2CAP security (on_sig 0x02, host.rs:934-956): for `psm != PSM_SDP` on a link that is
   not encrypted, answer Connection Response result=1 (pending), status=1 (authentication
   pending). Push the Chan with a new state `WaitSecurity` (keep `ident`, `rcid`) and call
   `start_security(li)`. In `advance()` (host.rs:709-735), when `enc`, send result=0 for every
   WaitSecurity chan on `a` with its saved ident, move it to Config and call `send_config_req`.
   In `security_failed` (host.rs:737-752), answer result=3 (security block) and drop them.
   On an encrypted link, behave as today.
6. Pairing gate (hardening, same lane or a follow-up): in IO_CAP_REQUEST, if no PAIR op is
   pending for `a` (`self.pairs`) and no outgoing channel to `a` is waiting, send
   IO_Capability_Request_Negative_Reply (0x0434) with reason 0x18. LINK_KEY_NEG_REPLY stays. Then
   an unbonded device cannot reach HID by just-works on an incoming link. An outgoing
   CONNECT/PAIR from bthid/btctl (an explicit user action) still pairs.

API (lib/bt-host/src/api.rs, bump the BT_SEAM.md table): event `0x86 LINK  bdaddr[6] up:u8
incoming:u8`. Send it to each owner in `self.listens` (dedupe) on link up and link down. Old
clients ignore unknown ops (bthid's `_ => {}`).

Knobs (userland/btd/src/main.rs:79-92, as `pub` fields on Host, defaults below):
- `--idle-ms N` (default 5000; was IDLE_DISCONNECT_MS)
- `--grace-ms N` (default 20000; idle limit for a fresh incoming ACL from a bonded device)
- const CROSS_WAIT_MS = 10_000

### bthid (userland/bthid/src/main.rs)
a. CONNECT failure (main.rs:451-454): if `ctrl` or `intr` is already set (the pad opened
   channels), only log `bthid: ADDR: outgoing PSM 0xNN failed (status S); keeping the device's
   channels`, clear `busy` and `outgoing`, and do not tear down. Else, if status is 7 (BUSY) or
   10, keep `outgoing = true` and set `retry_at = now + 2000` (one retry per start; field
   `retries`). Else tear down as today.
b. EV 0x86 LINK up for a listed address (`listed()`) with no ctrl/intr and not busy: set
   `wait_until = now + INCOMING_WAIT_MS (3000)`. The main loop (poll timeout already 2 s; drop it
   to 500 ms while any wait/retry is armed) calls `start(a)` when the deadline passes and ctrl is
   still None. Connecting over the existing ACL is what `ensure_link` does: it returns the Up
   link, then advance -> security -> L2CAP. LINK down: clear the timers.
c. EV_INCOMING (main.rs:468-482): set `outgoing = false` and cancel `wait_until`/`retry_at`.
   If the slot (ctrl for 0x11, intr for 0x13) is already filled and `hid.is_none()`, close the new
   cid (`self.bt.close(cid)`, log `duplicate PSM 0xNN, keeping the first`) instead of
   overwriting. If `hid.is_some()` it is a genuine reconnect: keep the current teardown.
   Also ignore (close) incoming channels from addresses that are not in `listed()`, until the
   btd pairing gate (6) lands.
d. CONNECT_RESULT success for a PSM whose slot is already filled (main.rs:457-466): close the
   new cid and do not chain the next connect.

## Test plan (Pi only for the race; host tests first)
Host, no QEMU: `cargo test -p bt-host --target aarch64-apple-darwin`. New Vc traces in
lib/bt-host/src/tests.rs:
- `crossing_conn_request_while_paging_is_adopted`: CONNECT 0x11 -> Create_Connection out ->
  feed Conn_Request(addr) -> expect Accept_Connection_Request (not Reject) -> feed Cmd_Status
  CREATE_CONN 0x0b -> no ConnectResult -> Conn_Complete ok -> Auth_Requested -> ... -> CONNECT_RESULT ok.
- `create_conn_0x0b_then_incoming`: Cmd_Status 0x0b first, then Conn_Request + Conn_Complete;
  same result. Variant without the incoming: ConnectResult BUSY after 10 s of ticks.
- `incoming_bonded_link_gets_grace_and_security`: Conn_Request + Conn_Complete for a keyed addr
  -> Auth_Requested sent, LINK event emitted. tick(+6 s) -> no Disconnect. tick(+21 s) -> Disconnect.
- `incoming_hid_psm_pending_until_encrypted`: L2CAP ConnReq 0x11 on an unencrypted link ->
  response result 1. Feed Encrypt_Change -> response result 0 + Config_Req. With auth failure
  -> result 3.
- Unbonded IO_CAP_REQUEST with no PAIR pending -> 0x0434 negative reply.
Plus `bttest` (userland/btd) still green.

Pi 5 (pi-remote skill; userland only, so deploy /bin/btd + /bin/bthid, keep the kernel):
1. Pad off. Boot. Pad on within 10 s of the `bthid: listening` line (reproduces the race), 5 times.
2. Pad on BEFORE power-up (the exact report), 3 times.
3. Pad on long after boot (pure device-initiated reconnect), 3 times.
4. With the pad on, `pkill bthid` and let init respawn it (outgoing to a live pad).
5. Negative: `btctl remove E4:17:D8:82:A2:0A`, pad on -> no evdev node, log shows the
   security block / IO-cap negative reply.
Pass: every run ends with `/dev/input/eventN` whose name is the pad, and with
`evtest`-style reads (or `cat /proc/bus/input/devices`) showing it. No `idle, disconnecting`
within 20 s of `connected`.

Expected log (race case):
```
btd: E4:17:D8:82:A2:0A: connect ...: status 0x0b, ACL exists, reusing the incoming link
btd: incoming connection from E4:17:D8:82:A2:0A            (or "... crosses our page; adopting")
btd: connected E4:17:D8:82:A2:0A handle 0x00b
btd: E4:17:D8:82:A2:0A: encrypted
bthid: E4:17:D8:82:A2:0A reconnecting      (device opened 0x11)  OR the CONNECT_RESULT path
bthid: E4:17:D8:82:A2:0A "8BitDo Lite 2" connected: gamepad (..)
```
Device-initiated case with a silent pad: `btd: ... encrypted`, then after 3 s
`bthid: connecting E4:..` (fallback over the existing ACL) and `... connected: gamepad`.
Diagnostic lines to add (keep them, they are cheap): `btd: {a}: link key requested, stored key
sent|none`, `btd: {a}: encryption on|off`, `btd: {a}: l2cap conn req psm 0xNN`. They decide RC4.
If the log shows `encrypted` and then no `l2cap conn req` for 3 s, the pad waits for the host
to connect, and bthid (b) is the fix. If `l2cap conn req` appears before `encrypted`, RC3 was
the only thing killing it.

## Order / risk
1 to 4 plus bthid a, c, d are the minimum fix (small, local). 5 and 6 close the HID injection
hole. Land them in the same lane if the host tests stay green: 5 changes timing for every
incoming non-SDP channel. b needs the API event. Without it, the bthid retry in (a) covers the
boot race but not "pad paged with no outgoing request pending".
