# lane/pigem-tx 2026-10-27: Pi 5 GEM transmits nothing

Status: PARTIAL (fixes for the likely causes plus a sharper diag; needs a Pi boot to confirm).

## What the boot said
`hw tx 0 | sw tx 2 | TSR 0x110 ... TBQP 0x1dc6020`, link 1000 full, RX fine (43 frames).
- TSR 0x110 decodes to bit 4 `tfc` (frame corrupted by a bus error) and bit 8 `hresp` (the DMA got an error
  response). It is not a missing clock: an unclocked transmitter leaves `tgo` set and never reports `hresp`.
- TBQP = ring base + 0x20: the GEM read descriptors 0 and 1 (16-byte reads work) and then failed on the
  frame-buffer reads. NCFGR DBW = 128 bit, so INCR16 bursts are 256-byte AXI reads.
- RX DMA writes to the same RAM and window work. So reads fail, writes work, small reads work and large
  reads fail. The likely cause is PCIe payload sizing. The root complex runs with `RCB_MPS_MODE`, so it
  splits read completions at its own MPS. RP1 is left at its reset MPS of 128 B with an MRRS of 512 B. Its
  256-byte read requests then come back as completions larger than RP1's MPS, RP1 rejects them as malformed
  TLPs, and the GEM sees an error response. Linux never meets this, because `pci_configure_mps` copies the
  root port's MPS into RP1.
- Also, after such an error the transmitter stays halted, because nothing cycled TE.

## Fixes (drivers/src/macb.rs, drivers/src/macb_hw.rs)
1. `pcie_fixup` runs before any DMA. It sets RP1 DEVCTL so that MPS = min(root port MPS, RP1 MPSS) and
   MRRS = MPS. It logs `[macb] pcie payload: rc devctl .. (mps N) rp1 devcap .. devctl old->new (mps N mrrs N)`.
   Knob: `macb.mps=0` only logs and changes nothing.
2. TX error recovery, in the style of Linux `macb_tx_error_task`. On TSR tfc/hresp/rle/und it clears TE,
   waits for `tgo` to clear, returns every slot to USED, rewinds head and tail to 0, clears TSR and sets TE
   again. The first 3 recoveries are logged with the failing descriptor. Knob: `macb.txrecover=0` turns it off.
3. `macb.burst=1|4|8|16` pins the AXI burst (the default is 16, as in Linux). `macb.burstauto=1` steps the
   burst down 16→8→4→1 on every second DMA error. That is opt-in, because it rewrites DMACFG while RX is running.
4. The diag line now prints at +10 s and again at +40 s. It adds the decoded `tsr`, TBQPH/RBQPH, Q1TBQP,
   `txerr`/`drop`/`burst`/`errtsr`, the failing descriptor (`errtxd`), descriptor 0 (`txd0`) and the pcie summary.
   (Unused queues already pointed at a dummy USED descriptor, and TBQPH was already written: both ruled out.)

## Evidence
- macb-tests 13/13 (`cargo test --target aarch64-apple-darwin`); 4 new tests (devctl, TSR decode of 0x110,
  burst field, ring reset). Commit 919552bf.
- `build-all.sh --arch aarch64 --rpi5` release: OK.

## Next Pi boot: what to look for
- Success: `pcie payload: ... devctl 0x..->0x.. (mps 256|512 mrrs 256|512)` (or `mps 128 mrrs 128`), then
  `diag +10s: hw tx N>0 ... isr tcomp >0 ... tsr comp ... txerr 0`, then a DHCP lease.
- `tx halted (TSR .. tfc|hresp)` repeating even after the fixup: try `macb.burst=4`, then `macb.burst=1`. If
  burst 1 works, the limit is the read size, not MPS. If nothing works, look at `errtxd` (word 2 must be 0x10,
  the 64 GiB DMA offset) and the RC inbound window for reads.
