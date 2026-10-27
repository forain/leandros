# lane/pisshrace 2026-10-27 — Pi 5 inbound SSH "Connection refused" over Wi-Fi: ROOT-CAUSED + FIXED (host test; Pi run pending)

## Root cause
- lane pirxwedge made wifi0's smoltcp stack late: `init_nic` runs from the daemon once brcmf
  knows its MAC (seconds into boot). `listen.rs`/`udpany.rs` still assumed "NIC stacks exist
  from boot": `listen()` put a 0.0.0.0 listener only on stacks that existed at that moment
  (`listen::create` returns false on a `None` stack, silently), and nothing ever added it later.
- So: sshd listening before wifi0's MAC => no LISTEN socket on wifi0 => every SYN over Wi-Fi
  gets an RST ("Connection refused"), ping fine, `/proc/net/tcp` still shows 0.0.0.0:22 LISTEN
  (that's the lo-stack socket). Before/after was a boot-timeline race; 30ac675c..d8b75390
  changed only drivers (BT now succeeds, RP1/PCIe probing), which flipped it. Every Pi
  observation fits; laptop/QEMU never hit it (iwlwifi/virtio know the MAC at probe).
- Same hole for wildcard UDP servers (udpany mirrors).

## Fix
- `servers/net/src/latestack.rs` (pure): Registry + `adopt`. listen() registers the lid before
  creating; `init_nic` calls `listen::adopt(sid)` + `udpany::adopt(sid)` after storing the stack
  and prints `[NET] wifi0: adopted N listener(s), M udp server(s)`. `create` is idempotent per lid
  (listen racing stack creation); destroy_all unregisters first and adopt removes what it added
  if the lid vanished (close racing adopt). udpany commits the mirror under MIRRORS only if the
  server is still registered there.
- `/proc/net/listen`: `<stack> lid N port P arm <State> queue Q` per listener per stack, then
  `registered lid N 0.0.0.0:22 on k/n [missing <stack>]...`.

## Tests
- host-tests 13/13 (5 new, real smoltcp): `legacy_late_stack_refuses_ssh` reproduces the RST
  without adopt; with adopt the SYN puts the arm into SynReceived, no RST; race cases.
- check-rpi5 rpi5 + raspi4b clean; aarch64 release build; aarch64 regress (HVF, full default suite incl. sshsmoke, wifisimtest, listentagtest): overall PASS, 0 boots failed.

## Open
- After `kill` of sshd on the Pi the Mac's SYN timed out (not refused). With the old code the
  restarted sshd should have landed on wifi0 too; not explained. Next boot: if it recurs, compare
  `/proc/net/listen` (arm state on wifi0) and `/proc/net/nic` (stack_rx, netd_why) before/after.
- bt pump (1 ms `pump_once` timeout) not measured; it parks via the transport wait, no lock seen held.
