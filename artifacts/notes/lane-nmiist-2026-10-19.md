# lane/nmiist (2026-10-19)

x86_64: NMI, #DF and #MC on dedicated per-CPU IST stacks. Closes the
residual risk lane kexecapcpu left open: the NMI gate had no IST, so an NMI
landing in `syscall_entry` before the stack switch (CS=0x08, RSP still the
user stack) pushed its frame through the user RSP (ring-0 #PF / SMAP ->
#DF). Both senders of NMIs were exposed: the AP-initiated kexec hand-off
(NMI to the BSP) and the watchdog stall probe.

## Design

- `gdt.rs`: IST1 = #DF (8 KiB), IST2 = NMI (16 KiB), IST3 = #MC (8 KiB),
  one stack per vector per CPU (.bss, page-aligned), set in `init_cpu`
  before the CPU's `lidt`. Separate stacks per vector because an IST switch
  always restarts at the slot's top: #MC inside the NMI handler (or #DF
  inside either) must not overwrite the outer frame.
- No guard pages: the kernel has none anywhere (task stacks are HHDM buddy
  blocks with a canary; the image is mapped with large pages shared by every
  address space and copied into the kexec identity map). Each IST stack has
  a canary in its lowest word; the NMI handler checks all three on exit and
  reports an overflow once.
- NMI nesting: not guarded, by construction. NMIs are blocked until the next
  IRET; the danger is an exception inside the NMI handler whose IRET reopens
  them. The handler cannot fault: no locks, no allocation, no user memory,
  and the probe's stack walk checks every word's page with a lock-free
  page-table walk (`kernel_va_mapped`, kernel half only) before reading it.
  #MC/#DF have their own stacks and halt. The doc comment on `exc_nmi` says
  a faulting path added later needs the Linux-style guard.
- GS: the NMI path has no `gs:` operand anywhere (CPU index from
  RDTSCP/TSC_AUX or LAPIC ID, probe slots are statics), so it does no
  `swapgs` and is correct in all GS states an NMI can see: ring 3, the
  pre-`swapgs` instruction of `syscall_entry` and the `restore_user_gs` ..
  `iretq` exit window (kernel CS + user GS), plain kernel. Documented that a
  future GS user there must decide from the GS_BASE MSR, not CS RPL.
- Watchdog probe: reads the interrupted RBP that `exc_nmi`'s prologue saved
  (now on the IST stack; bounds-checked against this CPU's NMI stack) and
  walks from the interrupted RSP taken from the hardware frame, only when
  that RSP is a mapped kernel address (the syscall-window NMI has CS=0 with
  a user RSP: no walk, sample still recorded).
- Unknown NMIs (e.g. a probe NMI arriving after the watchdog's 2 ms wait
  cleared the request) are logged (`[NMI] ... unknown NMI ... ignored`, max
  8) and ignored. Before, they halted the kernel, or from ring 3 called
  `exit_group_signal`, which from IST context would schedule away from the
  IST stack. #DF/#MC now always print and halt (`fatal_ist`), never kill a
  task for the same reason (a machine check from ring 3 used to kill the
  task; there is no MCE recovery, so halting is the honest outcome).
- kexec logs `[KEXEC] NMI stack high-water N of 16384 B` (the AP-initiated
  hand-off runs `final_bsp` on the NMI stack).

## Tests (Mac, TCG, CoW clones of main's images, release kernels)

| run | result |
|---|---|
| x86_64 q35 4 CPUs, `wdog.selftest=1`, 4 x `yes` + `cat /proc/uptime` loop | login; `[WDOG] cpu1 took no timer tick ...`, `cpu1 NMI sample: kernel RIP=...` in `sched::wdog_selftest_stall`, bt 2 frames (walk through the IST frame works) |
| x86_64 chained kexec from cpu1, cpu2, cpu3 under the same load | 3/3: `stopping 3 other CPU(s) from cpuN (AP): NMI to the BSP`, `BSP cpu0: INIT sent`, 4 CPUs + login every generation; NMI stack high-water 1152 B of 16 KiB |
| aarch64 TCG boot (regression, code is x86-only) | 4 CPUs, login |

No `[NMI]` unknown/canary lines, no kernel exceptions in any log.

Test-harness note: the driver's `log` only holds output read while a
driver command was attached (QEMU's socket chardev drops the rest); the
complete serial stream is `/tmp/leandros-<RUN_ID>-serial-full.log`. The
watchdog selftest output arrives between commands, so grep the full log.

## Not done
- The syscall-window hit itself is probabilistic and was not observed to
  occur in these runs (the load is there to make it possible; there is no
  instrumentation that proves a sample landed in the window).
- KVM / real hardware: same code path, not run here.
