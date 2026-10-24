# lane/usbladder — 2026-10-23 (L8.2 / L8.3)

Status: PARTIAL (fixes merged; the atomic-context case has no in-guest red/green guard; the not-ready-LUN wait is still open).

## Commits
- 0faae273 early xHCI quiesce, soft-lockup detector, MSC retry back-off, first stall guard (WIP)
- b1bb7fbc in-guest stall fault knob, 5 s ladder for atomic callers, early-quiesce log line

## FIXED / DONE
- L8.3 early xHCI quiesce (`usb_hcd::early_quiesce`, called right after PCI init on both arches). Boot log on aarch64:
  `[XHCI] 00:0a.0 early quiesce: idle (bus mastering already off)`. QEMU firmware leaves BME off, so the
  halt/reset path was not exercised in QEMU. It needs a real-PC boot.
- Soft-lockup detector (`sched::softlockup_tick`): no `soft lockup` or `atomic context` line appeared in any
  regress log (0 files).
- Retry back-off between ladder attempts (50 to 800 ms). With preemption off, `delay_ms` does nothing.
- Atomic callers: `ScsiDisk::set_atomic_probe`. A request where the caller may not sleep now gets a 5 s budget
  (one attempt) instead of 30 s. The real atomic callers are `klog::flush_final` (reboot/power-off; it holds the
  klog STATE spin::Mutex) and `klog::flush_panic`. Each flush is one header write plus data writes, so on a dead
  stick a reboot used to spin for about 30 s per write. The periodic klog `flush_job` masks IRQs but is sleepable
  (it parks). Host guard: `dead_stick_atomic_caller_gives_up_after_one_attempt` fails with a 30 s atomic budget
  and passes with the fix (msc-tests 27/27).

## Stall guard
- The old QEMU `block_set_io_throttle` model is REFUTED as a guard: QEMU cancels the request synchronously and
  freezes the vCPU (`attempt 1 failed ... 127904 ms`).
- New knob `usbms.stall_lba=L usbms.stall_ms=N`: the first request covering sector L opens an N ms window in
  which every xHCI bulk transfer waits out its deadline and returns Timeout.
- `usbstalltest.py` uses `stall_lba=25600 stall_ms=20000` on a 1-CPU guest. Result: 4 failed attempts plus a
  reset recovery, then success on attempt 5 after 20.4 s. Worst shell RTT 0.6 s. The stalled read returns rc=0
  and later I/O is fine. PASS.
- This is a SMOKE test only. Main cannot be made red with it, because a read() caller is sleepable on any kernel
  (`atomic=false` in the fault line). The atomic case is covered by the host test.

## Open
- `ScsiDisk::wait_ready` still waits about 10 s for each not-ready LUN, one LUN after another, in `usb_storage::init`.
- Out of scope: the Limine HHDM gap above 4 GiB with Fast Boot off; aarch64 has no HHDM walker.
- The early-quiesce halt/reset path needs a real PC where firmware leaves the xHCI as a bus master.

## Tests
aarch64 full regress: 43 suites green; sshsmoke was red once (a known flake) and passed 1/1 on re-run.
x86_64 release build OK.
