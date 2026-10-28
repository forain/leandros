# lane/pibtreconn 2026-10-28: bonded HID pad reconnect (Pi 5, 8BitDo Lite 2)

Design: artifacts/notes/design-pibtreconn-2026-10-28.md (all items implemented).

## Changes
- lib/bt-host/src/host.rs: page crossing (CONN_REQUEST adopts a Connecting link; Create_Connection
  0x0b / crossing failures are not fatal, 10 s wait then BUSY); incoming links from bonded devices
  get start_security plus a 20 s idle grace (`--grace-ms`), others keep `--idle-ms` (5 s); the idle rule
  no longer fires on security traffic; incoming non-SDP L2CAP on an unencrypted link answers
  "pending, authentication" (new chan state WaitSecurity) and completes on encryption or is refused
  with a security block; IO_CAP_REQUEST with no PAIR/outgoing channel is refused (0x0434, 0x18);
  diagnostics: link key requested, encryption off, l2cap conn req psm.
- lib/bt-host/src/api.rs, docs/servers/bluetooth.md: event 0x86 LINK (bdaddr, up, incoming) to LISTEN owners.
- userland/btd: `--idle-ms N`, `--grace-ms N`.
- userland/bthid: BUSY retry once (2 s), LINK-triggered fallback connect after 3 s of silence, failed
  outgoing connect never tears down an existing session, duplicate PSM channels closed, incoming from
  unlisted addresses closed.

## Tests
`cargo test -p bt-host --target aarch64-apple-darwin`: 16 pass (6 new: crossing adopt, 0x0b then
incoming (+ BUSY after 10 s), bonded grace and security, pending until encrypted (+ security block),
unsolicited pairing refused).

## Not covered
The race itself needs the real pad; see the lane report for the Pi trial state.
