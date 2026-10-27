# lane/pirxwedge 2026-10-27 — Pi 5 wedge at `wifi0: carrier up`: ROOT-CAUSED + FIXED (code + host replay; Pi run pending)

## Root cause
`servers/net/src/lib.rs` (before this lane):
- `init_nic` (~3020): `let Some(mac) = nif.mac() else { return };` — stacks are built once at
  net init. brcmf learns its MAC from the firmware later (`drivers/src/brcmf/netdev.rs:175`,
  inside the bring-up kthread), so **wifi0 never had a smoltcp stack** (iwlwifi/virtio know
  their MAC at probe, so the laptop and QEMU never hit this).
- net_daemon per-netif pass: `let Some(ref mut s) = *stack else { continue };` skipped wifi0,
  but after the pass `for nif in iter() { again |= nif.rx_rearm(); } if again { continue; }`
  — brcmf `rx_rearm` = `!RXQ.is_empty()`. Once brcmf queued one AP frame (dp=rx / dp=on
  after carrier up), nothing ever popped RXQ and the daemon restarted passes forever
  without parking. netd is a kernel task (`sched::spawn`), IRQs masked between parks.
- On cpus=1: the only CPU runs that loop with IRQs off: no tick, no wedge scan, brcmf
  kthread never runs again, shell dead. The loop holds no TrackedMutex (no Spin probe)
  and sets no stage marker, and the tick watchdog needs a peer CPU => total silence.
- Fits every observation: dp=off never queues to RXQ (frames dropped in brcmf with
  carrier held) => alive; dp=rx => `[NET] wifi0: carrier up` (track_carrier runs before
  the stack lookup) then immediate wedge. Evidence to look for in old logs: no
  `[NET] wifi0: stack up (brcmfmac)` line ever.
- 4-CPU silence: same loop on one CPU; why the other CPUs/watchdog also went quiet is not
  proven (netd touches the lo stack, INJECT, IFRT, NFTABLES every pass; pre-piwedge WDOG
  printed through OUTBOX).

## Fix (9e42385a..)
- `servers/net/src/rxpolicy.rs` (pure, host-tested): per netif Poll / InitStack / Discard.
  The daemon builds the stack as soon as `nif.mac()` is Some (prints `[NET] wifi0: stack up`),
  DHCP starts via the link generation; until then and while admin down, it drops up to
  256 frames/pass (`nostack_drop` counter).
- `RearmGuard`: at most 64 back-to-back rearm passes, then a 1 ms park, one
  `[NET] rx rearm capped: ...` line; `/proc/net/nic` `netd_why ... rearm_capped N nostack_drop N`.
- `sched::kthread::cond_resched()` at the top of every daemon pass + new wedge slot
  `netd` (SLOT_NETD=4): set at the first non-parking pass, cleared at each park, so a daemon
  that stops parking for 3 s is reported by the tick scan even on one CPU.
- Detector limit (documented): a loop that never opens an IRQ window on the only CPU cannot
  be reported; that needs an NMI-class source (BCM2712 GIC-400 has no pseudo-NMI; FIQ/group 0
  belongs to the secure firmware). Hardening rule instead: every kernel-task loop that can
  repeat without parking must call cond_resched().
- `brcmfctl dump [N]` (default 8, max 64): next N received data frames as
  `[brcmf] rxdump <i> len <L> <hex first 64 B>`, any dp mode (use with dp=off);
  `cfg::parse_dump_line` reads them back for host replay.

## Tests
- NEW `servers/net/host-tests` (`cargo test --manifest-path servers/net/host-tests/Cargo.toml
  --target aarch64-apple-darwin`): 8/8. Daemon model around real rxpolicy + vendored smoltcp
  with the kernel's features; AP frames: ARP bcast, IPv6 RA, MLDv2 query (HbH), DAD NS,
  STP LLC, SNAP/CDP, 802.1Q, mDNS, SSDP, DHCP offer, IGMP query, IPv4 fragment, EAPOL,
  truncated IPv4, odd-length unknown ethertype, 10-byte runt, empty frame.
  Guard shown failing on the old policy: `legacy_daemon_never_parks_with_frames_for_a_stackless_netif`
  (100 000 passes, 0 frames read); fixed: parks after <= 2 passes, all frames through smoltcp,
  DHCP DISCOVER transmitted. Each frame type x500 into a live stack terminates.
- brcmf-tests 73/73 (dump parse/format round trip). check-rpi5 rpi5 + raspi4b clean.
- aarch64 QEMU regress (HVF, full default suite): overall PASS, 0 boots failed.

## Next Pi session
1. Boot (kexec) => `[WEDGE] armed`, wifi0 associates. Expect, at carrier up:
   `[NET] wifi0: carrier up` then `[NET] wifi0: stack up (brcmfmac)` and no wedge.
2. `brcmfctl dp rx` (if still in dp=off): shell stays alive; `cat /proc/net/nic` shows
   wifi0 `stack_rx` climbing, `netd_why ... rearm_capped 0`.
3. `brcmfctl dp on`: DHCP lease (`[NET] wifi0: address ...`), `ping` the gateway.
4. Optional capture: `brcmfctl dp off; brcmfctl dump 32` => `[brcmf] rxdump` lines; save
   them (dmesg) for replay tests.
5. If anything wedges: expect a `[WEDGE] netd stage stuck` or `[NET] rx rearm capped` line now.
