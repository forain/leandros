# lane/pikexecwifi 2026-10-27: brcmf after kexec + pi-boot kexec -e: FIXED (code + mock bus; Pi run pending)

## Causes
1. brcmf after a kexec from a kernel whose brcmf was up/associated: the CYW43455 keeps running
   across kexec; the only reset was `sdio_host::boot_probe` -> `probe(20_000, ...)`
   (drivers/src/brcmf/sdio_host.rs:805, WL_REG_ON low 20 ms) and the old kernel never powered the
   chip down (kernel/src/power.rs quiesce_phase1 had no brcmf step). The chip attached
   (`chip 4345 rev 6 cores 7`) but the post-download HT request timed out: stale chip state.
   Not hardware-proven (no Pi access); the card-kernel kexec working (chip never had firmware) fits.
   ctl: the init-failed park DOES poll /etc/brcmf.ctl (brcmf_boot.rs run loop, 1 s); why
   `seq=501 retry` printed nothing is not explained by the code (VFS read is coherent; diag is
   bounded). Now the ctl wait line is printed after every failure, so its absence is a signal.
2. pi-boot: `(sleep 1; /bin/kexec -e) </dev/null >/dev/null 2>&1 &` (scripts/remote.py:864) runs
   in the session's process group; userland/sshd/src/session.rs channel_close (~line 627) and
   Conn::drop (~line 103) send SIGHUP to `-pid` when the channel closes -> the sleep dies, kexec -e
   never runs. (The laptop flow runs /bin/reboot in the foreground, which is why it works.)

## Fixes (bc3560d0)
- drivers/src/brcmf/power.rs (pure): WL_OFF_MIN 100 ms floor (also for `retry wloff_ms=`),
  boot/retry default 200 ms, one auto-retry with 1 s off after a `chip`/`fw`/`clm`/`ver` failure.
- brcmf_boot.rs: auto-retry in the run loop; `shutdown()` = netdev::request_shutdown (brcmf thread:
  disassoc + WLC_DOWN, 500 ms bound) then `sdio_host::power_off()` (host reset, clock/power off,
  WL_REG_ON low, held across the jump). Called from kernel/src/power.rs quiesce_phase1 (rpi5 only)
  for kexec, restart and power-off.
- remote.py pi_kexec: `kexec -e` in the foreground (ssh timeout 20 s = success path); works against
  the old deployed userland too. `KEXEC NOT EXECUTED: ...` when kexec -e returns, or when after the
  timeout the old kernel still answers with /tmp/.remdbg-alive present (reports same/changed uname -v).
- sshd unchanged (OpenSSH does not SIGHUP non-pty children on close; worth aligning later).

## Tests
- brcmf-tests 76/76 (+3): StaleChip mock (HT only after WL_REG_ON low >= N): old policy (20 ms,
  no retry) fails with `fw HT clock timeout`; new policy recovers on the auto-retry; floor; quiesced
  old kernel; retry count bound; retry decision table.
- check-rpi5 rpi5 + raspi4b clean. aarch64 QEMU regress (HVF): overall PASS, 0 boots failed.

## Expected on the Pi
Old (fixed) kernel at `kexec -e`: `[REBOOT] brcmf: disassoc+down in N ms; sdio2 host reset, SD clock + bus power off, WL_REG_ON low`
(and `[brcmf] shutdown: disassoc ok WLC_DOWN ok`). New kernel: `[sdio] sdio2 base=... wloff=200ms`, then
`[brcmf] chip 4345 rev 6 ...` and init ok. From an unfixed old kernel, if HT still times out:
`[brcmf] fail fw HT clock timeout stage=...`, `[brcmf] auto-retry #1: ... (WL_REG_ON low 1000 ms)`,
a second `[sdio] ... wloff=1000ms`, then init ok. pi-boot: no more silent 240 s wait.
