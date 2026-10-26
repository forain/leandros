# lane/pv3d-power 2026-10-26 (openbugs #98, V3D phase A) - power-up + identity

Status: DELIVERED (code + host tests + compile checks + aarch64 QEMU regress). Not yet run on a Pi.

## What landed
- `drivers/src/v3d/` (compiled everywhere; hardware glue only under `aarch64 + rpi5`):
  - `regs.rs`: the `V3dRegs` seam (verbatim from the design note §4), the `V3d { regs, ver, va_width }`
    struct phase B consumes, and the 7.1 register map (hub, MMU, TFU, GMP, core CTL/INT/CLE/PTB/CSD, SMS,
    BCM2712 PM, firmware clock tags). CLE/PTB/CSD offsets are upstream values not used yet; phase C re-checks.
  - `power.rs`: `power_on(regs, env)`: PM `0x304 = 0x5a000000 | (PM & 0xffffff) | V3DRSTN` with 1 us
    settles and a readback check -> firmware clock 5 SET_CLOCK_STATE / GET_MAX / SET_RATE(max) -> SMS
    `TEE_CS = CLEAR_POWER_OFF` poll IDLE -> SMS `REE_CS = 4` poll not ISOLATING/RESETTING -> CTL_IDENT0
    "V3D" magic -> `L2TFLSTA=0, L2TFLEND=~0` -> MMU_DEBUG_INFO, HUB_IDENT0-3, CTL_IDENT1/2 -> require
    ver 71 and 1 core. No V3D register is touched before PM + clock succeed. Polls are bounded at 100 ms by the
    clock AND by a 200k-iteration cap (a frozen clock still ends).
  - `mod.rs`: `probe_boot` (ioremaps hub/core/sms/PM/mailbox as Device), `DEVICE: Mutex<Option<V3d<MmioRegs>>>`,
    `REPORT`, `PROBE_STATE` (0 none, 1 up, 2 skipped, 3 no map, 0x100+step failed). `mmu.rs`/`irq.rs` are
    one-line stubs owned by pv3d-mmu.
- `rpi_mailbox.rs`: `set_mbox_virt()` so property calls work after boot through an ioremapped mailbox
  (the HHDM does not map it then); the 16-line reply dump is skipped on that path.
- `kernel/src/init.rs`: one line, rpi5 only, after the i915 probe and before `klog::init`.
- `drm_device_interface.rs`: stub `V3D_HUB_IDENT1` 0x0700_0101 ("V3D 1.0") -> 0x0002_0117 (TVER 7,
  REV 1, NCORES 1, WITH_TFU). Mesa never reads it.
- Knob: `v3d.probe=0` on the kernel command line skips the probe.

## Evidence
- `cd drivers/v3d-power-tests && cargo test --target aarch64-apple-darwin`: 14/14. Covers sequence order,
  PM password and bit preservation (junk in [31:24] on readback cannot corrupt the password), PM not taken,
  4 clock failure modes, SMS power/reset timeouts (bounded ~100 ms, nothing after), frozen clock
  (iteration cap), ident magic/version/cores rejection, field decoders, and the stub encoding.
  The stub guard fails on the old value (shown red, then fixed).
- `scripts/check-rpi5.sh` and `CHECK_FEATURE=raspi4b`: both clean, no warnings in v3d code.
- aarch64 QEMU regress (virt; rpi5 code compiled out, pure modules compiled in): every case green except
  `/bin/killmt 40 proclife`, which timed out once (wall 5633 s, a host stall) and passed on a re-run under
  the lock (33.2 s). The `--arm-v3d` kmscube guard: `Rendered 29 frames in 0.296 sec (97.9 fps)`, rc 0.

## Expected Pi boot lines (exactly one `[v3d]` outcome line)
- Success: `[v3d] ident ver=71 rev=6 cores=1 qpus=8 va=32 pa=NN clk=NNNMHz sms=idle`
  followed by `[v3d] regs hub_ident=...,...,...,... core_ident=...,...,... mmu_debug=0x... pm=0x...->0x... t=NNus`.
- Failures (all inert; boot continues, DRM stays on the stub):
  - `[v3d] fail map ioremap` - KVA window could not map a region.
  - `[v3d] fail pm mismatch val=0x<PM readback> pm=0x<before>->0x<after> clk=0MHz` - V3DRSTN did not stick.
  - `[v3d] fail clk-state firmware val=0x<state> ...` - SET_CLOCK_STATE refused or reply bit1 (no such clock).
  - `[v3d] fail clk-rate firmware val=0x<max or 0> ...` - GET_MAX or SET_RATE refused/zero.
  - `[v3d] fail sms-power timeout val=0x<TEE_CS> ...` - SMS never reached IDLE in 100 ms.
  - `[v3d] fail sms-reset timeout val=0x<REE_CS> ...` - stuck in ISOLATING_FOR_RESET/RESETTING.
  - `[v3d] fail ident mismatch val=0x<CTL_IDENT0> ...` - core block does not say "V3D" (unclocked?).
  - `[v3d] fail ver mismatch val=0x<HUB_IDENT1> ...` / `[v3d] fail cores mismatch val=0x<HUB_IDENT1> ...`.
  - `[v3d] skipped (v3d.probe=0)`.
  A `[MBOX] TIMEOUT ...` line before a `clk-*` failure means the mailbox itself did not answer.
- Unprotected risk: if the firmware leaves V3D unpowered in a way the PM write does not fix, the first SMS
  access could take an external abort (panic with ESR/FAR) rather than a clean fail line. Recovery:
  `v3d.probe=0`.

## Open
- Not hardware-verified. No kiostat counter exists in the tree; state is `drivers::v3d::probe_state()` and
  the dmesg line.
- Phase B (pv3d-mmu) takes `DEVICE`; GET_PARAM still answers from the stub (phase C switches to `REPORT`).
