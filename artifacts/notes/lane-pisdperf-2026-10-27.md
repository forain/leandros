# lane/pisdperf — 2026-10-27 — PARTIAL (code done, hardware win unproven, merge pending regress)

## A. SD throughput (drivers/src/sdhci.rs, blkdev_sdhci.rs)
- Contiguous spans go out as ONE CMD18/CMD25 with Auto CMD12 (Transfer Mode bit 2, multi bit 5), up to
  128 sectors = 64 KiB per command and per DEVICES hold; read_block/write_block are one 8-sector command.
- blkdev_sdhci::write_blocks now reaches the driver as a span (was 4 KiB write_block calls).
- Any multi-block failure: reset CMD/DAT, manual CMD12 (abort type), reset, redo the span sector by sector
  (old path). Logs `[sdhci] multi-block write|read failed (N<<16|E latched: XXXXXXXX), single-sector redo ok`.
  3 such fallbacks -> `[sdhci] multi-block disabled; I/O is single-sector PIO from now on`.
- Boot line: `[sdhci] I/O mode: PIO multi-block CMD18/CMD25 + auto-CMD12, up to 64 KiB per command (single-sector fallback)`.
- No DMA (SDMA/ADMA2 not attempted: budget). Next step if PIO multi-block is < 10 MB/s on hardware.

## QEMU raspi4b (TCG, 64 MiB dd urandom + sync, sha256 after reboot)
- before (main 910d71e7): write 1247 ms (guest clock), read-after-reboot 612 ms.
- after: write 2057/1849/2163 ms, read 1450/1459/1954 ms; sha256 identical after reboot every run,
  zero fallbacks. Without cond_resched: 1722/1357 ms (so not the yield).
- So QEMU shows correctness only and is ~1.5-2x SLOWER in emulation (cause not found; QEMU's SD model has no
  per-command program latency, which is what single-sector CMD24 pays on a real card, ~1 ms/sector = 0.5 MB/s).
  If the Pi gets slower too, the multi path is the suspect: revert to single-sector by setting MULTI_ON false.

## B. Fairness
- span_io drops DEVICES and calls sched::kthread::cond_resched() between 64 KiB chunks (no-op in atomic
  context; preempt_check skips if a spinlock is held), so a long polled writeback yields the CPU when the tick
  asked. f2fs still writes under F2FS_MOUNTS (IoMutex) — any task needing f2fs waits for that hold; not changed.

## C. pi_sync.py
- extract{N}.sh runs detached: `trap '' HUP` + stdio to a log + `.done` marker, idempotent `.started` start,
  polled with short ssh calls (30 s each); budget max(1800 s, bytes/200 kB/s). pi-sync-qemutest.sh all ok.
- UNPROVEN: a guest test that kills the ssh client mid-job (scratchpad pisdperf/huptest.sh) found neither the
  HUP-ignoring job nor the control alive — inconclusive (test harness may not have started the jobs; `ps`
  output came back empty). Needs one manual check: on the Pi/guest, start
  `sh -c 'trap "" HUP; sleep 30; : > /tmp/x' </dev/null >/dev/null 2>&1 &` in an ssh session, kill the client, check /tmp/x.

## Tests
check-rpi5.sh (rpi5 + raspi4b) compile clean; raspi4b write/readback x4 ok; pi-sync-qemutest all ok.
Box aarch64 regress queued (rf-lane_pisdperf-aarch64-22261), result not seen by the lane.
