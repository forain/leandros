# Debugging

The tools and signals for finding out what a LeandrOS guest is doing — especially when it appears to be doing nothing — and the hazards that make an investigation chase a defect that is not there.

## When a boot prints nothing

Check, in this order, before touching kernel code:

1. **QEMU's stderr.** A launch refusal is not a guest hang. Since QEMU 11.1, `-accel hvf` on a GICv2 machine exits immediately with `HVF does not support GICv2 emulation`; every AArch64 boot on a Mac silently failed this way for over a week before it was read. The driver writes stderr to `/tmp/leandros-qemu-stderr.log`.
2. **Image provenance.** A `--rpi5` or `--raspi4b` build overwrites `leandros-limine-aarch64.img`, and that kernel faults on `virt` before printing a byte ([building](../getting-started/building.md#image-provenance-pitfalls)). `run-qemu.sh` warns about images older than the source.
3. **The CPU model.** Under TCG, AArch64's `-cpu max` advertises FEAT_LPA2 and Limine 11.4.1 wedges in its final hand-off, PC fixed on one instruction with the kernel entry in `x0`; the launchers pass `lpa2=off` ([running](../getting-started/running.md#accelerators)).
4. **Kernel stack overflow.** Kernel stacks have no guard page. A canary in the lowest word of every kernel stack is checked on syscall return and prints `*** KERNEL STACK OVERFLOW *** pid=… syscall=…`; the build refuses frames over 48 KiB.
5. **Bisect with serial markers.** Early-boot hangs have been located by writing single characters to the UART at each stage of the entry assembly and `kernel_main` ([boot](../kernel/boot.md)).

## Stall diagnostics: `[WDOG]` and `[TIMER]`

Every CPU counts its local timer ticks. A CPU that takes none for 2 s is reported by a live one on the raw UART, repeated every 10 s and kicked with a reschedule IPI each scan:

```
[WDOG] cpuN took no timer tick for ~2 s ...: pid=P (/bin/x) last syscall 0x.. preempt_disable=.. [spinning for LOCK held by cpuM]
```

- `pid=0` — the CPU is idle with a dead timer (the hypervisor lost the virtual-timer edge); a `[TIMER] cpuN virtual timer silent … re-armed` line follows once the IPI runs its idle loop.
- `pid=P in syscall` — an interrupts-off spin in the kernel. When the lock is one of `RUN_QUEUE`, `PIPEWIRE_STATE`, `FD_TABLES`, `PIPE_RINGS`, `VIRTIO_GPU`, `PORT_TABLE`, `EPOLL` or `ADDRSPACE_BUSY`, the line names it and its holder ([`sched/src/lockwatch.rs`](../../sched/src/lockwatch.rs)).
- Only a stall of **all** CPUs at once prints nothing. Then the host is the instrument: sample the QEMU process (`sample <pid>` on macOS) to see which vCPU threads spin in the hypervisor trap and which sleep in WFI.

See [scheduler](../kernel/scheduler.md) for the watchdog's design.

## Ctrl-T task dump

Typing **Ctrl-T** (byte `0x14`) on the *serial* console dumps the system, SysRq-t style ([`servers/tty/src/lib.rs`](../../servers/tty/src/lib.rs)). It is intercepted before the line discipline, so it works with no foreground process group — the one way to ask a wedged userspace where its threads are parked. It prints, per task: state (`Running` = spinning, `Blocked` = parked in a syscall), CPU, blocked port, futex word value (for a musl lock, the holder's tid) and the user-mode frame; then the buddy-allocator census, the AF_UNIX socket tables (`[SOCK]`/`[CONN]`/`[BOUND]`), and census lines registered by servers (`[FDK]` fds per kind, `[DRMH]` DRM handles, epoll pools, the workqueue). With `sched::pcsample::ENABLED` set ([`sched/src/pcsample.rs`](../../sched/src/pcsample.rs)), it also gives a per-process summary of sampled user PCs.

To symbolize user addresses: position-independent executables load at `0x200000` and `ld-musl` at `0x30000000` — symbolize with that bias, not base 0. Kernel addresses resolve against `target/final-<arch>/kernel` (`nm`, `llvm-addr2line`).

## Never poll the HMP monitor under HVF

On a Mac with HVF and the in-kernel GICv3, HMP `x` and `info registers` — anything that needs `run_on_cpu` on a vCPU — hang QEMU's main loop **permanently** about once in a few hundred calls, even against an idle guest. The monitor stops answering, `quit` and SIGTERM are ignored (SIGKILL works), and because every virtio device is serviced by the main loop the guest sees dead devices: `[GPU] control-queue TIMEOUT`, frozen audio, `[PW] producer gap`, `fork`/`exec` blocked on virtio-blk. **The probe manufactures exactly the whole-guest wedge it was meant to diagnose.** A "silent wedge" investigation in September 2026 did not reproduce in 27 runs once the probe was removed.

Use guest-side liveness instead: the `[WDOG]`/`[TIMER]` lines, a userspace heartbeat, or `liveness-run.py` ([workflow](workflow.md#tools-built-on-the-driver)). Serial output still works during a main-loop hang (the PL011 transmit path runs on the vCPU thread); serial *input* does not.

On TCG and KVM hosts the monitor remains safe and useful: check that the kernel's tick counter advances (`x /gx <symbol address from nm>`, using `x` with kernel virtual addresses, not `xp`), sample `info registers -a` and symbolize the PCs, and walk the run queue. Two live tasks (init and a shell) with idle CPUs is a system at its prompt, not a hang.

## Serial capture pitfalls

Three independent mechanisms make an absent line on the serial log *not* evidence of an absent event:

- **QEMU drops serial output while no client is connected** (`server,nowait` sockets). A test that outlives a polling client prints into the void, the log stops growing, and the run looks like a kernel hang. One afternoon was spent on a phantom x86-64 hang in `racetest` that had in fact finished. Keep one persistent reader for anything longer than a few seconds (`liveness-run.py`, `scripts/scmrun.py`, `driver.py run`), or read QEMU's own log file.
- **On x86-64, output can land on the framebuffer console instead of serial.** The framebuffer is the primary console; a command whose stdout is that console prints nothing on serial, and `driver.py cmd` returns an empty string that looks like a crashed binary. Check serial first, then take a `driver.py screenshot`; only if both are empty is there a real failure.
- **Interleaving.** Kernel diagnostic lines used to be written piece by piece without the console lock and were spliced character by character into user output — `[FOPRasKs]wo rtdg: id=4` is `Password: ` woven into `[FORK] tgid=4`. Harnesses missed prompts and "lost" a root login one boot in ten; no input byte was ever lost. Kernel lines are now staged per CPU and written whole through an 8 KiB outbox by whoever holds the console lock (interrupt context never spins on it); 0 splices in 30 boots per architecture afterwards. A staged line with no trailing newline waits until its CPU prints one, and kernel lines can still be dropped under host back-pressure, by contract. See [console](../graphics/console.md).

Harness-side rules that follow:

- Do not anchor a prompt regex at the end of the buffer: kernel `[FORK]` lines print after the prompt, so every command waits out its timeout (a 1 s dialog once measured as 11 s). Search for the prompt after the echoed command.
- Do not match a completion marker that also appears in the command line; the tty's echo matches first.
- When captured output contains `38;5;14m`, `ESC 7`/`ESC 8`, `[?25h` or a stray `78` (a cursor-position-report tail), suspect the harness before the kernel ([methodology](methodology.md)).
- A guest has no `grep`; a probe built on it reports "no matches" for everything.

## Injecting input

- **Pointer: use QMP `input-send-event` with absolute axes, never HMP `mouse_move`.** `mouse_move` emits relative events; the only pointer is an absolute-only virtio tablet, so QEMU's input core finds no handler and drops them without a diagnostic. 280 accepted `mouse_move` commands once produced zero guest events — indistinguishable from a broken driver. `info mice` shows the tell: `QEMU Virtio Tablet (absolute)`.

  ```json
  {"execute":"input-send-event","arguments":{"events":[
    {"type":"abs","data":{"axis":"x","value":12000}},
    {"type":"abs","data":{"axis":"y","value":9000}}]}}
  {"execute":"input-send-event","arguments":{"events":[
    {"type":"btn","data":{"down":true,"button":"left"}}]}}
  ```

- **Key chords need QMP.** HMP `sendkey` presses and releases in one shot, so Ctrl+Alt+Fn has no HMP form. `driver.py chord ctrl alt f2` and `driver.py wheel` use QMP; both `run-qemu.sh` and the driver always attach a QMP socket.
- Use `evtest2` in the guest as the destination probe: it reports what actually arrived.
- Re-capture the screen some seconds later before calling keystrokes lost, and reboot between experiments after a `kill -9` of a compositor that held DRM master.

## Changing the syscall and exception assembly

The hand-written entry and exit paths (`arch/x86_64/src/syscall.rs`, `arch/aarch64/src/exception_asm.s`) use tricks such as "do not reload the saved `rax`, trust the live register". Inserting a function call between where a value is set and where it is trusted clobbers it with no compiler warning, and the symptom can be arbitrarily delayed: one such change booted, ran `execve` and several `write`s correctly, and hung on the first interactive `read`.

After any change to these files:

1. Do not stop at "it boots". Exercise an interactive round trip whose result depends on a return value (a shell command that checks `getpid()`), on both architectures.
2. If anything looks wrong — a garbage PID, a hang, silence — build a clean baseline to compare against rather than reasoning from the assembly: `git stash push -- <changed files>`, `./scripts/build-all.sh --arch <one>`, boot, then `git stash pop`.

A related trap: a missing `cld` on kernel entry left `RFLAGS.DF` set after musl's backwards `memmove`, so kernel copies ran backwards — found only through Firefox crashing on x86-64.

## Other instruments

- **Gated debug output.** Most diagnostics are compile-time constants defaulting to off (render and audio hot paths, `[MMAP]`/`[CPIO]` tracing, EL0 fault backtraces, unix-socket traces, `DRM_STATS` in [`drivers/src/drm_device_interface.rs`](../../drivers/src/drm_device_interface.rs) for the `[DRMSTAT]` census). Turn one on in a private build; leave it off on `main`. After gating any helper, diff a live serial log before and after — a private copy of a print helper in one server once kept printing unlabelled hex digits after the shared one was gated.
- **`ENOMEM` sites.** Every `ENOMEM` from the memory, address-space and exec paths goes through `enomem_site()` with a gated, budgeted trace naming the site ([design principles](../overview/design-principles.md#an-errno-must-name-its-own-cause)).
- **Clock drift.** `clockdrift.py` measures guest time against host time. The guest clock was tick-derived and 5–36 % slow on every accelerator until 2026-09-16; any wall-time baseline recorded before then (settle times, `sleep` durations, MAME run times, `[WDOG]` intervals) is stale. On x86-64 the boot log prints the TSC frequency every clock derives from, once (`[TSC] … MHz (cpuid …); pit measured … MHz`); it is also `cpu MHz` in `/proc/cpuinfo`. See [time](../kernel/time.md).
- **Host sleep.** A Mac on battery idle-sleeps and freezes the guest; the runner holds the Mac awake, but a stall of several minutes with nothing in the guest log is worth checking against `pmset -g log`.
- **Audio.** `LEANDROS_AUDIO_WAV` records what the guest plays; check per-second RMS, not file growth — a streaming-but-silent client grows the file too.

## See also

- [Methodology](methodology.md) — controls and the instrument-was-the-defect cases.
- [Workflow](workflow.md) — the driver and the liveness tools.
- [Kernel invariants](../kernel/invariants.md)
- [Scheduler](../kernel/scheduler.md) — watchdog, lock tracking.
