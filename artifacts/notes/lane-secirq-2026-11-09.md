# lane secirq 2026-11-09 - seccomp filter IRQ window inside one filter

FIXED. Interrupts are masked in evaluate only because syscalls run with IRQs masked (no kernel
preemption); there is no lock. The chain is kept alive by the slot head (head is only replaced by a
node containing the old one via prev), so no refcount change is needed to open windows mid-run.

- lib/seccomp-bpf: `run_ticked` / `run_stack_ticked` call a tick closure every TICK_INSNS (256)
  instructions and between filters; `run`/`run_stack` unchanged.
- sched/src/seccomp.rs `evaluate` passes `irq_window` as the tick.
- userland/seccomptest: `max_len_filter_threads_tsync` - 4096-insn filter (LD + 4094 ALU ADD + RET ALLOW),
  4 threads x 400 getpid, main TSYNCs a second filter mid-run and measures a 1 ms sleep-loop gap.
  aarch64/HVF: PASS, max gap 4 ms. Limits: a 4096-insn run is only tens of microseconds, so the
  test cannot fail on the old code by latency; it guards correctness and TSYNC-vs-running-filter safety.
