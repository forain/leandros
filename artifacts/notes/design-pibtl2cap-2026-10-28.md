# design-pibtl2cap-2026-10-28: no L2CAP on the pad's incoming ACL (Pi 5, 8BitDo Lite 2)

Trial f51a46029ffb (main 81c94950). Read-only diagnosis; no code changed.

## Evidence (Pi, 2026-10-28)
- No ACL trace exists: btd has no trace flag (args: --dev --name --fd --sock --keys --idle-ms
  --grace-ms), bt-host logs no ACL and no signalling except conn req (0x02), and /dev/hci0 has a
  single owner (EBUSY), so no sniffer can be attached. **Raw ACL bytes could not be captured.**
- 200 incoming cycles in dmesg (3193 lines): `l2cap conn req psm` = 0 times, `channel psm` = 0,
  `timeout in WaitConnRsp` = 200. The pad never sent a Connection Request that btd parsed.
  **(a) refuted**: the "pending" branch (host.rs:1066) logs "l2cap conn req" first; it never ran.
- `btctl connect E4:17:D8:82:A2:0A 1` (SDP, needs no security) over a live incoming link: timeout.
  Any L2CAP stack answers an SDP Connection Request, so this is not pad policy. **(d) refuted as
  the root cause**: even if the pad wants to open 0x11 itself, it would still answer us.
  So ACL does not flow on that link in at least one direction (most likely both: the pad's own
  0x11 request / info request never shows up either).
- Restarting btd (init restarts it; HCI_Reset, fresh credits): `ready ... acl=1021x8`, bthid pages
  the pad (OUTGOING, we are central) -> encrypted -> psm 0x11 + 0x13 open -> event2. Reproduced
  twice. The first session then ended `reason 0x08` (supervision timeout); every link after it
  was INCOMING (pad pages; or a crossing adopted as incoming) and every one failed identically.
- Commands/events keep working throughout (link key reply, encryption), and no `hci write
  failed/stalled` or kernel `rx drop`/garbage lines: the UART and /dev/hci0 command path is fine.

## Root cause (narrowed, not yet proven)
ACL is dead on incoming links. Two candidates remain and the data cannot split them, because
every incoming link so far came after a failed/dropped session:
- R1 host ACL tx stall: `acl_avail` stuck at 0 (pump_acl host.rs:909-919 stops before the
  first L2CAP frame). Credit math reads sound (link_down returns `outstanding`, host.rs:872-876),
  but it is unobservable today, and one stale-link path exists: a new CONN_COMPLETE for an addr
  that still has an Up entry (seen in the log: two `incoming connection` with no `disconnected`
  between) leaves two Up links; `up_link()` (host.rs:765) returns the OLD handle, so advance()
  (host.rs:803) sends our requests on a dead handle (no NUM_COMPLETED -> credits leak too) - (b).
- R2 incoming/peripheral-specific: something on the incoming path (ACCEPT_CONN role 0x01 at
  host.rs:413, crossing adoption, incoming-link security from pibtreconn) leaves the ACL unusable.
- (c) info/feature exchange: the pad cannot be waiting on our Information Response unless our
  tx is dead, since we answer 0x0a immediately (host.rs:1198); folds into R1.

## Fix design
1. Trace knob (needed to finish the diagnosis and for every future BT bug):
   - lib/bt-host/src/host.rs: add `pub trace: bool` to Host (set from btd `--trace` /
     `BTD_TRACE=1`, userland/btd/src/main.rs:87-102 arg loop).
   - on_acl (host.rs:931): `if self.trace { self.log(format!("acl< h=0x{:03x} pb={} {}", h, pb, hex(&data[..data.len().min(32)]))) }`
     before link_by_handle, and log `acl< unknown handle` when link_by_handle fails (today a silent return).
   - pump_acl (host.rs:917, before `self.out.push_back(pkt)`): `acl> h=.. avail=.. hex(pkt[..min 32])`.
   - pump_acl when `*pool == 0` and the queue is non-empty: one rate-limited `acl tx blocked: 0/N credits, q=..` line.
   - NUM_COMPLETED (host.rs:513): trace `ncp h=.. n=..`; credit_back (921): log when no Up link matched.
   - on_sig (host.rs:1048): trace every code/ident/len (info req 0x0a included).
   - timeout line (host.rs:1350): always append `h=0x.. avail=a/N outstanding=o txq=q links_up=k`.
2. Stale-link fix (R1/(b)), host.rs CONN_COMPLETE success path (~line 441): before marking link i Up,
   `if let Some(j) = self.links.iter().position(|l| l.addr == a && l.state == LinkState::Up && l.handle != new_handle) { self.log("..stale link 0x.. dropped"); self.link_down(j, status::LINK_LOST); }`
   (fix index i after removal). Also make up_link() prefer the newest (max up_at) Up link.
3. Credit self-heal (R1 safety net), host.rs:1350 timeout path: if `acl_avail == 0` and
   `links.iter().all(|l| l.outstanding == 0)`, log `ACL credit leak, restoring N` and set
   `acl_avail = info.acl_pkts`, then pump_acl().
4. If the trace shows R2 (acl< frames from the pad never arrive on an incoming handle, credits fine):
   try ACCEPT_CONN role 0x00 (become central, host.rs:413) as the next A/B, and log the
   ROLE_CHANGE event; and check whether start_security on an incoming link (pibtreconn) sends
   AUTH_REQUESTED while the pad is already encrypting.

## Test
Ship 1-3 (userland only: btd + bt-host, no kernel). Repro needs the pad PAGING US (blinking):
turn the pad off and on (or let it time out out of range) while btd runs. Do it twice:
(i) right after a btd restart with the pad off (fresh incoming, splits R1 vs R2), and
(ii) after a connected session ends with 0x08. Expect `acl>`/`acl<` lines with real bytes; pass =
psm 0x11 + 0x13 open on an incoming link and event2 appears.

## State left on the Pi
btd was killed twice and restarted by init normally (no debug flags). Pad was connected
(outgoing), then `btctl disconnect` -> pad went to sleep; it is now off/idle, not blinking.
