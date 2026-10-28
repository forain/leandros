# lane/pigem-ring 2026-10-27: Pi 5 GEM transmits 2 frames, then stalls

Status: PARTIAL (fixes for every plausible cause plus diag; needs a Pi boot to confirm).

## What the boot said (main 1069c63c, pcie.rpi5=1, polled)
`hw tx 2 | sw tx 6 | TSR 0x21 (comp|ubr) | TBQP ring+0x20` and, overlooked so far, **RX stalls too**:
`hw rx 112, RBQP slot 0x31 -> 0x70, sw rx 34 at both +10 s and +40 s`. smoltcp calls `recv` on every pass
(the DHCP retransmits prove passes ran), so the CPU read slot 34 as "not USED" for 30 s while the GEM had
filled up to slot 112. Both directions fail in the same way: the CPU and the GEM do not agree on the
descriptor pages.

## Root cause (most likely): cached lines of the descriptor pages behind the NC alias
On the Pi (NON-COHERENT mode) the rings are vmap'ed Normal-NC, but `zeroed_page` wrote each page through the
cacheable linear map and only CLEANED it (`dc cvac`). The lines stayed valid in the cache. The architecture
allows an access with mismatched attributes to hit such a line (loss of coherency). NC reads then return the
stale line (RX head never sees USED), and NC writes update the line instead of DRAM (the GEM keeps reading
USED in TX slot 2, so it gives UBR forever, although TSTART is rung every second). Linux avoids this:
`arch_dma_prep_coherent` cleans AND invalidates before the NC remap.
TSTART retries were already there (send_packet and a 1 s retry), so candidate 1 is refuted. ISR handling is
not involved: reclaim reads the descriptors, not the ISR.

## Fixes (drivers/src/macb.rs, macb_hw.rs)
1. `desc_page`: clean + invalidate the linear alias (`sync_for_cpu` after the clean) before the NC map.
2. `wmb()` = `dsb st` before every TSTART (Linux `wmb()`). `fence(SeqCst)` is only `dmb ish`.
3. The ring rewind (error recovery, from lane pigem-tx) now rewrites TBQPH/TBQP to the ring base, as Linux
   `macb_tx_error_task` does. Before, it reset the software ring to 0 under a GEM parked on slot k.
4. TX stall watchdog (`hw::TxStall`): frames in flight, TGO clear, and TBQP/tail unmoved for 3 polls (3 s)
   lead to a rewind, logged as `tx stalled: TBQP slot .. head .. tail .. desc ..`.
5. diag adds `txring head/tail/hw slot/stalls ctrl0-3`, `rxring head/hw slot/frags/rxd[head]`.

## Evidence
macb-tests 15/15 (2 new: ring_slot, TxStall). The new tests cover new logic, so there is no failing
pre-fix run. build-all --arch aarch64 --rpi5 release OK, QEMU aarch64 (HVF) /bin/nettest 1/1 PASS. A stall rewind is skipped while the link is down (TE is off on purpose then).

## Next Pi boot: what to look for
- Success: diag +10s `hw tx N>2`, `txring ... stalls 0`, `rxring head == hw slot` (or close to it), sw rx keeps
  rising, a DHCP lease on eth0 from 192.168.2.1, and a lease visible on the Mac's bridge101.
- `stalls >0` with `ctrl0-3` showing USED (bit 31) on posted slots: the NC writes still do not reach DRAM.
  Next suspect is the vmap WC attribute on the Pi; try Device-nGnRE (Uncached) for the desc pages.
- RX head stuck while `rxd[head]` shows bit 0 set: the CPU sees USED but does not consume it. Look at netd.
