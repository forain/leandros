# lane lapdispC2a (2026-10-28): display C2 prerequisites that need no monitor

Branch lane/lapdispC2a. Everything is off by default (knobs below); nothing lights a display.

## What is in
- `drivers/src/i915/power.rs`: power-well refcounts for PW2 and DDI_B/C/D_IO (CTL2 request/state bits 2i+1/2i, 1 ms
  wait). `outputs::with_pw2` now goes through it. A well the BIOS already requested is never released.
- `drivers/src/i915/wrpll.rs`: Linux skl_ddi_calculate_wrpll / multipliers / params_populate, verbatim, plus CFGCR1/2 packing.
- `drivers/src/i915/buf_trans.rs`: `_skl_trans_hdmi` (11 entries, default 8, written to HW slot 9), `_kbl_u_trans_dp`,
  `_skl_trans_dp`, DISPIO_CR_TX_BMU_CR0 iboost field. Tables fetched from Linux v6.6 intel_ddi_buf_trans.c.
  NB: the design said "entry 9"; Linux's HDMI default is table entry 8, which is written to hardware slot 9.
- `drivers/src/i915/ddb.rs`: the pure DDB arithmetic moved out of kms.rs (`skl_ddb_plan`, `pipe_limits`, `ddb_entry`).
  `kms::ddb_setup` no longer refuses when another pipe is lit: pipe A gets `[0, 892*wA/sum)`; it still keeps the
  firmware allocation if a lit pipe's own plane range would overlap A's new range (we do not drive that pipe yet).
- `drivers/src/i915/pipeb.rs` (kthread `i915-pipeb`, skipped on the fake device):
  - `i915.pipeb_test=pll`: DPLL1 for 148.5 MHz, wait lock, log lock time, disable, restore CTRL1 field and CFGCRs.
    Refuses a PLL that is enabled or mapped to a DDI.
  - `i915.pipeb_test=dryrun`: same, plus the whole pipe-B HDMI register plan (steps 0-12 of design C2) logged as
    `pipeb: plan 0x<reg> <- 0x<val> <what>` and never written; takes and puts PW2 + DDI_x_IO.
  - `i915.pipeb_test=hdmi1|hdmi2`: logs that the light-up is C2b; does nothing.
  - `i915.ddb_test=1` (`kms::ddb_test`): pipe A plane 1A shrinks to [0,438) (A's half minus the 8-block cursor slice) with
    watermarks recomputed (skl_plane_wm), held 10 s counting underruns, restored. Armed by a PLANE_SURF rewrite in a
    vblank-evaded window. Runs before any client flips; skipped if a flip already happened.
  - `i915.pipeb_port=1|2` (default 1 = DDI-B), `i915.pipeb_dpll=1|2|3` (default 1).
- Host tests: `cd drivers/i915-tests && cargo test --target "$(rustc -vV | sed -n 's/host: //p')"` (82 pass; wrpll for
  148500/74250/108000/65000/25175/140000 kHz, buf_trans, ddb).

Design corrections found while porting: 148.5 MHz gives p=12 (p0 2 p1 3 p2 2, dco 8910000 kHz), not "p0 2 p1 1 p2 5";
WM_LINETIME_B for 1080p60 is 119 (DIV_ROUND_CLOSEST(2200*8000/148500)), not 59.
