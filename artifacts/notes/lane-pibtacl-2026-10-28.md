# lane/pibtacl 2026-10-28: BT ACL trace, stale-link drop, credit recovery (design items 1-4)

Design: design-pibtl2cap-2026-10-28.md (same folder). Userland only (bt-host, btd); kernel untouched.

## Delivered
1. Trace: `btd --trace`, env `BTD_TRACE=1`, or marker file `/etc/btd.trace` (init-independent; the marker survives
   how init spawns btd). Logs `acl< h pb hex`, `acl> h avail hex`, `ncp h n avail`, `sig h code id len`,
   `acl< unknown handle`, `acl tx blocked: 0/N credits, q=..` (rate-limited, always on), and the channel timeout
   line now ends `h=0x.. avail=a/N outstanding=o txq=q links_up=k` (always on). Frames capped at 32 bytes of hex.
2. CONN_COMPLETE drops older Up BR/EDR links for the same address (`stale link 0x.. dropped for new 0x..`);
   `up_link()` prefers the newest `up_at`.
3. Credit leak: on a channel timeout, if `acl_avail == 0` and no link has outstanding credits, log
   `ACL credit leak, restoring N` and reset the pool; the queue is pumped.
4. ACCEPT_CONN role knob: `btd --accept-central`, `BTD_ACCEPT_CENTRAL=1` or `/etc/btd.accept-central` (role 0x00).
   Default unchanged (0x01, peripheral).
Tests (`cargo test -p bt-host --target aarch64-apple-darwin`): 18 pass, incl.
`new_conn_complete_drops_stale_up_link_for_same_addr` and `acl_credit_leak_is_restored_on_channel_timeout`.

## Pi
pi-sync of the rpi5 build onto trial f51a46029ffb (good 8e2fd94ff6e7 untouched), 45 changes, 1291 s.
`/etc/btd.trace` created; btd restarted by init: `[btd] trace=true accept_central=false`. No ACL line yet
(the 8BitDo was asleep). Remove with `rm /etc/btd.trace`.

## Next
After the pad pages us: pull dmesg and look for `acl<` on the incoming handle (R2 vs R1), `ncp`, and the timeout line.
If credits fine and no `acl<` arrives, touch `/etc/btd.accept-central` and restart btd.
