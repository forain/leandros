# lane/pirp1-bar 2026-10-27 — RP1 BAR reads all-ones (FIXED, pending Pi boot)

Symptom (Pi 5 D0, main 910d71e7, pcie.rpi5=1): config space works (1de4:0001), but
`[rp1] chip_id=0xFFFFFFFF`; BARs printed bar0=0x1f00840000 bar1=0x1f00800000.

Root cause (by elimination; host-tested, not yet hardware-verified):
- Those BAR values cannot come from our placement (largest-first from PCI 0 would put the 4 MiB
  BAR1 at 0 and BAR0 at 0x41_0000), so assign_rp1_bars placed nothing: the sizing read-backs were
  not valid masks (e.g. the BAR writes did not land), every request was filtered out, `end` stayed 0
  and the root-port memory window (type-1 0x20) was computed from `end` → written CLOSED. A closed
  window makes every BAR access a UR → MISC_AXI_READ_ERROR_DATA = all-ones. This alone explains the
  symptom regardless of where the BARs sit.
- Secondary: root-port command MEM/MASTER was a 16-bit write; now dword.
- Address translation was correct: BARs hold PCI addresses (window CPU 0x1f_0000_0000 → PCI 0).

Fix (`brcmstb_hw::setup_rp1_bars`, pure + CfgSpace seam): size BAR0..2 with dword writes, fall back to
known RP1 sizes (16K/4M/64K) on an invalid mask, place at the DTB layout, READ BACK every BAR, open the
window over the BARs as they really are, RC cmd |= MEM|MASTER, RP1 cmd |= MEM. rp1::probe maps the
recorded BAR1 (no live re-size). New log line `[pcie] rc window=.. rc_win=.. rc_cmd=.. rp1_cmd=..`;
per-BAR `(mask=..)` / `(nowrite orig=..)` flags say which hypothesis held.

Evidence: drivers/brcmstb-tests 17/17 (3 new, config-space recording mock incl. the dropped-write
case reproducing the boot's BAR values; asserts the old logic yields a closed window), rp1-tests 7/7,
check-rpi5.sh rpi5 + raspi4b clean, aarch64 regress (see merge).

Open: sub-word config writes on bus 1 (enable_mem_and_bus_master, MSI-X control) remain 16-bit;
if `[rp1] msix` fails next boot, convert pci.rs's backend writes to dword RMW.
