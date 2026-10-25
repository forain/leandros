# lane/blkdead, 2026-10-25: slow virtio-blk request killed the disk; page-in EIO was SIGSEGV; coldstart-bench leaked QEMU (openbugs L12 #50, #51)

## Verdict: FIXED (#50 both halves, #51). Commits 988f4876 (knobs + guard, red), 80fc911e (fix), 94ba0481 (coldstart-bench).

## #50a virtio-blk: a slow request is no longer fatal
- Cause: `wait_for_completion` gave up after IO_TIMEOUT_NS = 30 s and `do_io` set `dead = true` for good. In the original
  log (/tmp/ffsession/cs-termsegv-x86_64-4/serial-full.log:282) device_status was 0x0f (DRIVER_OK, no NEEDS_RESET), so
  the device was fine and only slow (loaded host, x86_64 TCG).
- Missed IRQ: not possible. The driver takes no completion interrupt and polls the used ring itself, so every poll
  already acts as a lost-interrupt check. Whether the original request completed later can't be known, because the
  ring was abandoned.
- Fix: keep waiting, as Linux virtio-blk does (no request timeout by default). Warn `[VIRTIO-BLK] slow request: ...
  unanswered for N ms ... still waiting` at the threshold and again each time the wait doubles, then log `completed
  after N ms`. The disk is declared dead only when device_status reads NEEDS_RESET, FAILED or 0xFF.
- Knobs: `virtio_blk.inject_delay=N:MS` holds back the doorbell of the N-th request, and the wait counts from submit.
  `virtio_blk.slow_ms=MS` sets the threshold.

## #50b page-in read error -> SIGBUS (BUS_ADRERR)
- `install_file_fault`: in an ELF-segment (non-MAP_EOF_SIGBUS) mapping, a failed or short read returned
  Fault::Segv. It now returns Fault::Bus, which goes through the existing FAULT_SIGBUS -> BUS_ADRERR path, as on Linux.
- Knob: `mm.pagein_eio=<comm>` makes every file page-in of that thread fail with EIO.

## Guard: host:blkslowtest.py (runtests suite `blkslow`, aarch64)
- Before the fix (988f4876): `request type 0 sector 1138448 timed out ...; disk marked dead`, then f2fs read-only:
  FAIL. With no delay injected, `/bin/hello` exit status was 139 (SEGV): FAIL.
- After the fix: the 3 s late request (1 s threshold) logged one warning, then `completed after 3000 ms (status 0)`.
  The disk stayed writable and hello exited 135 (SIGBUS): PASS.

## #51 coldstart-bench leaves QEMU running
- Cause: any exception (the login failure, "no shell prompt") or signal skipped the stop at the end of main().
- Fix: main() now wraps the run in try/finally. SIGINT/TERM/HUP raise SystemExit, the finally runs `driver stop`, and
  the QEMU pid is killed if it survives. CS_KEEP=1 keeps only a successful run's VM. A VM that was already running
  for the run id is left alone. `CS_FORCE_FAIL=login` simulates the failure.
- Forced failure: the old script exits 1 with QEMU LEFT RUNNING; the new one exits 1 with no QEMU left. SIGTERM 30 s
  into a run gives exit 143 and no QEMU left.

## Regression
- aarch64 regress: every suite passed except /bin/nettest icmp_poll_timeout and icmp_recv_eintr (an ICMP packet
  arrives when none is expected). It reproduced on a rerun. It is environmental: lane scanfrag's runs have failed
  the same 2 cases since 02:34, and its branch has none of these changes. Still open, not this lane.
- x86_64 coldstart (TCG, virgl, CS_STEPS=term): login 0.9/2.5 s, term 2.8/3.6 s, 0 PF/SEGV/panic, and QEMU was
  stopped at the end.
