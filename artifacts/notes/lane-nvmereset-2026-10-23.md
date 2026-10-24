# lane/nvmereset — NVMe controller reset after a command timeout (L8.4) — FIXED (QEMU)

## Change
- `drivers/src/nvme.rs`: `CtrlState::exec` now wraps a reset-free `submit`. On a
  timeout or CSTS.CFS it logs `[NVME] nvmeN: controller reset (attempt n/3, k since boot)`,
  runs `reset()` (CC.EN=0, wait RDY=0, zero + re-program the admin queue, enable,
  Set Features NumQueues, re-create I/O CQ/SQ 1 on the same memory) and re-submits the
  command (only one is ever in flight; the bounce buffer still holds its data).
  `MAX_RESETS` = 3 per command; a failed reset, all-ones registers, or the 4th timeout
  marks the controller dead as before. Resets are off during bring-up and shutdown
  (`can_reset`).
- New host test `.claude/skills/run-leandros/nvmefaulttest.py`, suite `--suite nvmefault`
  (own boot, ~2 min, not in regress): 64 MiB random image on `-device nvme`, QMP
  `block_set_io_throttle bps_rd=4096` stalls a 16 MiB read past the 30 s timeout; once the
  reset line appears the throttle is lifted; the read must return the exact pattern and a
  later read of another region must work, with no "marked dead".

## Evidence
- Red (main's nvme.rs, aarch64 HVF): `TIMED OUT after 30000 ms ... controller marked dead`,
  stalled read wrong hash, later read `controller dead` (empty hash). FAIL nvmefault.
- Green aarch64: reset after 31 s, `controller reset done in 186–1105 us`, 16 MiB read
  pattern intact, later I/O ok. PASS nvmefault, PASS blkfault, regress PASS.
- x86_64 TCG: regress PASS, nvmefault PASS (reset done in 342 us). After the APST re-apply: aarch64 nvmefault + blkfault PASS.

## Notes / open
- The QEMU NVMe reset drains in-flight I/O (`nvme_ctrl_reset` -> drain), so a blkdebug
  `break` would deadlock QEMU; throttling is the stall that a reset can recover from.
- dd on `/dev/nvme0n1` without `iflag=fullblock` returns short reads (8 x 1M read 2 MiB;
  32 x 256k read 512 KiB) — legal for read(2), but worth knowing.
- APST is turned off again after a reset when bring-up turned it off (features revert to defaults on reset).
- Real-laptop confirmation of a reset is still open (no way to provoke a timeout there).
