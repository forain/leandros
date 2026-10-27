# lane/pv3d-tfu-csd 2026-10-26 (openbugs #98 phase D): SUBMIT_TFU + SUBMIT_CSD on the Pi 5 V3D

Status: DELIVERED (code + host tests + compile checks + aarch64 regress). Not yet run on a Pi.
Design: `lane-piv3d-design-2026-10-26.md` section 2 (TFU/CSD bullets), section 3 (uapi), row D. Builds on
`lane-pv3d-cl-2026-10-26.md`.

## Decision: one in-order pipeline, not per-queue retirement
TFU and CSD jobs are new `JobKind`s in the existing `ClQueue`: one job in flight across bin/render/TFU/CSD,
strictly in submission order. Fence ids are allocated under the queue lock, so fence order == queue order ==
retirement order, and the single global watermark (`syncobj_retire_fence`, `v3d_fence_done`) stays exact.
Why not per-queue tracking: the watermark cannot say "TFU fence 7 done, render fence 6 not"; making it
per-queue touches every waiter (WAIT_BO, SYNCOBJ_WAIT, poll, flip fences) shared with the virtio paths.
Deadlock-free: every in-sync a job can name already has a fence id older than the job's own, so in-order
execution always reaches it. Cost: no TFU/CSD overlap with bin/render (same class as the existing
"no bin(N+1)/render(N) overlap" throughput item). Revisit only if a Pi profile shows TFU/CSD serialisation.

## What landed
- `queue.rs`: `JobKind::{Cl, Tfu(TfuRegs), Csd([u32;7])}`, phases `Tfu`/`Csd`.
  TFU (hub, Linux `v3d_tfu_job_run` order): IIA, IIS, ICA, IUA, IOA, IOC (7.1), IOS, COEF0-3 (7.1 always
  writes all four), `ICFG = icfg | IOC` last. Done = hub TFUC, delivered into the queue as `EV_TFU_DONE`
  (bit 31) via a new hub hook. Timeout = reset (TFU has no progress register; Linux does the same).
  CSD (core): invalidate; QUEUED_CFG1..6; CFG7 = 0; CFG0 last. Done = CSDDONE bit 6. Every CSD job runs the
  cache clean (TMUWCF, L2T clean) before its fence = Linux's CACHE_CLEAN job, enforced in the queue (not by the
  caller). Zero workgroups in any dimension: not started (HW reads 0 as 65536), clean + fence still happen.
  Hang check: `CSD_CURRENT_CFG4` (0x968) batch counter must move every 500 ms.
- **Bug fixed in regs.rs**: TFU ICA/IIS were swapped. Linux v3d_regs.h 7.1: ICA = 0x710, IIS = 0x714
  (the design table had them backwards). Nothing used them before this lane.
- `submit.rs`: `TfuArgs` / `CsdArgs` (88-byte uapi, `v71.ioc` @80, CSD pad after coef), validation (only
  the EXTENSION flag; ext pointer iff flag), `plan_syncs_single` (MULTI_SYNC walk shared with CL via
  `walk_multisync`; all in-syncs gate the whole job, any wait_stage; legacy in/out_sync otherwise).
  GET_PARAM on the real device: SUPPORTS_TFU=1, SUPPORTS_CSD=1 (with CACHE_FLUSH=1, MULTISYNC_EXT=1 v3dv's
  `device_has_expected_features` is satisfied).
- `drm_device_interface.rs`: SUBMIT_TFU/CSD go to `v3d_real_submit_tfu/csd` only when
  `v3d::submit::active()` (aarch64+rpi5, device up); otherwise the unchanged `v3d_enosys`. CSD perfmon != 0
  -> ENOENT. BO refs held to the fence, out-syncs + BO fences attached (shared `v3d_real_enqueue`).
- Boot self-test (after the CL clear test, same `v3d.selftest=0` knob): 64x64 R32F raster -> UIF_NO_XOR TFU
  conversion. **TFU cannot write raster** (Mesa `v3dX(tfu)` refuses a raster destination), so raster->raster
  was not possible; the check is layout-free: source pixel i = `0x7C000000 | i`, the 16 KiB destination must
  hold all 4096 values exactly once. The CRC of the tiled output is printed for reference.

## CSD self-test: not hand-built; es31test plan instead
A hand-made CSD dispatch needs V3D 7.1 QPU machine code (ldunif, TMUD/TMUA store, thread-end thrsw + delay
slots). 7.1 changed the ALU encodings, and without Mesa's `qpu_pack.c` assembler run on the host the words
would be guesses; a wrong shader means a boot-time GPU hang + reset. Plan (pv3d-mesa / F): `/bin/es31test`,
GLES 3.1 on the v3d megadriver: SSBO of 4096 u32, `layout(local_size_x=64)`, dispatch (64,1,1), shader writes
`data[gl_GlobalInvocationID.x] = 0xC5D00000u | id`, `glMemoryBarrier`, map + check every word, print
`es31test csd ok 4096/4096`. That exercises SUBMIT_CSD, CFG0-6, CSDDONE, the clean and the fence end to end.
Optional host-side shortcut later: build Mesa's `broadcom/qpu` + `compiler` for the host and dump the
compiled CS (`V3D_DEBUG=cs`) into a generated `selftest_csd.rs` like `selftest_cl.rs`.

## Evidence
- `cd drivers/v3d-tests && cargo test --target aarch64-apple-darwin`: 62/62 (11 new: TFU 7.1 sequence,
  CSD sequence + clean-after, clean timeout fails the job, zero-WG dispatch, fence order across CL/TFU/CSD
  with stray early TFUC/CSDDONE, TFU/CSD deps, CSD batch-counter hang + TFU timeout reset, TFU/CSD uapi
  layouts, single-queue MULTI_SYNC plans, self-test register block + permutation check).
- Mutation guards (each red): no clean after CSD (3 fail), old swapped IIS/ICA (1), TFU/CSD routed as CL (6).
- `scripts/check-rpi5.sh` rpi5 + raspi4b: clean, no warnings in v3d / drm_device_interface.
- aarch64 regress (Mac, HVF, under the qemu lock; the box farm sat waiting for a slot and was cancelled):
  overall PASS, 0/1 boots failed; `--arm-v3d` kmscube guard `Rendered 29 frames in 0.298 sec (97.2 fps)`, so the
  QEMU stub path (SUBMIT_TFU/CSD still ENOSYS there, TFU=1 CSD=0 stub params) is unchanged.

## Next Pi boot (after `[v3d] selftest clear ... ok`)
- `[v3d] selftest tfu copy ok perm=4096/4096 crc=0x........ tfuc=1 t=NNus`
Failures (inert): `[v3d] selftest tfu fail timeout tfuc=0 tfu_cs=0x.. hub_int=0x.. t=..` (no TFUC in 200 ms;
job reset, BO leaked on purpose); `[v3d] selftest tfu fail mismatch perm=N/4096 dup=D foreign=F px0=0x..`
(ran but wrote wrong data: foreign=4096 = nothing written / wrong IOA; dup>0 = tiling/stride fields);
`... fail nomem|nova|map|queue-full`. With `v3d.selftest=0` neither self-test runs.
