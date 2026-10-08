# Methodology

How claims about LeandrOS are established, and the lessons — mostly refuted premises — that shaped that practice. The common thread: what separated a recorded cause from the truth was never more reasoning. It was instrumenting at the destination, a control that varied exactly one thing, and a test shown to fail before it was trusted.

## Prove the instrument first

Four separate defects in this tree were at one point recorded with the wrong cause and evidence beside them, and in three of the four *the instrument or the harness was the defect*:

- a serial reader that wedged the guest's timer tick, and then reported input loss;
- a completion marker matched from the tty's own echo of the command, so every read ended before the test had printed anything — later rows read the previous row's exit status, and nothing was timing out;
- a probe that reported "no matches" because the guest has no `grep`.

The pattern has recurred often enough to be a rule:

| Recorded premise | What it actually was |
|---|---|
| "The audio validation wedges the whole guest" | The HMP monitor probe (`x`, `info registers`) hanging QEMU's main loop under HVF, about once in 300 calls; 0 wedges in 27 runs without it ([debugging](debugging.md#never-poll-the-hmp-monitor-under-hvf)) |
| "Root serial login loses input one boot in ten" | Kernel log lines spliced character by character into `Password:`; no byte was ever lost |
| "The boot-time compositor never polls its input / greeter login fails under permission enforcement" | The driver records serial only while a reader is attached; the compositor registered its input queues 2–3 s after boot, unseen |
| "Coreutils: 51 pass, 54 fail" | The harness returned before the command ran and deleted the output line glued to a bracketed-paste escape; true score 104/105 — nearly filed as a kernel regression against the GICv3 port |
| "Pointer injection reaches no client" | HMP `mouse_move` is relative and the tablet is absolute; QEMU dropped all 280 events silently |
| "`drmsmoke` framebuffer sweep is racy" | The test compared a device-wide count that the greeter's compositor also changes |
| "A 1 s dialog takes 11 s to open" | A prompt regex anchored at end of buffer, defeated by kernel lines printed after the prompt |

Before trusting a number, ask what the instrument does to the system and what it cannot see; before trusting a test, watch it fail.

## Measure at the destination

Instrument where the effect is supposed to land, not where its cause is assumed to be:

- **"Keybindings never fire" was not input loss.** Input, XKB and configuration were fine; the bound action forked, and the fork deadlocked the compositor because the kernel ignored `CLONE_PARENT_SETTID` (musl threads had tid 0). The same bug was behind a blank Zink desktop and part of "iced apps don't paint".
- **"Zink runs at 0.65 fps with 2 s input stalls" was zero frames.** There was no scanout path for host-memory buffers; flips went to a resource over guest address 0. Once fixed: 20 fps. An earlier "pixels proven" record had run on the broken path.
- **Acceleration can move a bottleneck instead of removing it.** With virgl, guest userspace CPU fell from 38.5 % to 3.5 % — but total busy time *rose*, because the cost moved into the kernel. The measurement that mattered was the whole system, not the component that was changed.
- **"A 140 MB kernel leak per compositor death"** was mostly an orphaned greeter-login process; the remainder was real, was found by attributing memory per death (`scripts/greeterstorm.py`), and has since been fixed.

## Controls that vary one thing

- **An A/B must differ in one variable.** A Zink regression was attributed to image staging because the comparison spanned two kernels *and* a staging sync; the same data images rendered at 0 fps on one kernel and 18.5 fps on the next. The staging change only altered process start-up timing — the trigger, not the bug.
- **Run the baseline.** Before blaming a change for a failure, run the same test on the unmodified kernel. `waittest`'s "kernel flake" failed identically on the untouched `HEAD` kernel; it was a race in the test.
- **Check what else a constant sizes.** The "`.data` cliff" — a boot failure on one host after `MAX_OPEN_FILES` went from 32 to 256 — was a correct bisection to the wrong quantity. The change moved `.data` by 386 KB *and* grew a structure built on the stack from 24 KB to 72 KB; only the second mattered (a kernel stack overflow). Padding `.data` by 600 KB booted fine; a larger stack booted the byte-identical failing image with zero faults.
- **Check the "unaffected" machine's log.** The host that "worked" was printing the same page-fault line the whole time and reaching a login prompt anyway. The asymmetry was survivability, not susceptibility.

## The accelerator is not the architecture

A Mac runs AArch64 under HVF (fast) and x86-64 under TCG (slow); a Linux PC is the mirror image. Every "AArch64-only" symptom measured on the Linux machine was really slow-target-only and reproduced on x86-64 on the Mac. Three bugs were mis-filed under an architecture name in one day: a terminal cursor-position-report desync, a DRM in-flight-present race (3/3 on x86-64/TCG, never on AArch64/HVF) and COSMIC's bring-up latency. A one-architecture pass on the fast target would have shipped the present race.

The converse holds too. An interrupt-context self-deadlock in the virtio-keyboard probe (a lock held across the whole probe, retaken by the timer tick) always existed; under HVF the probe fits between ticks, under cross-architecture TCG it spans many and the collision is near-certain. The fast host was lucky, not correct. **Always name the accelerator, and treat a slow accelerator as a race amplifier worth running on purpose.** The same discipline applies to clocks: the guest clock's drift was blamed on HVF and turned out worst on TCG ([time](../kernel/time.md)).

## QEMU cannot catch ARM hardware bugs

"Test both architectures in QEMU" is necessary and **not sufficient**. The Raspberry Pi 5 bring-up hit five bugs in a row that emulation cannot show in principle:

- **TCG implements `LDXR`/`STXR` regardless of cacheability.** With the data cache off, every spinlock works in QEMU and deadlocks on silicon.
- **`-kernel` loads an ELF and honours `e_entry`.** Real firmware loads a flat binary and branches to byte 0, so section order inside `.text` matters.
- **QEMU's memory map is the model's, not the board's.** Addresses inherited from `virt` (RAM at `0x40000000`, MMIO in the first gigabyte) are wrong on a Pi, and the firmware silently refuses to load a kernel linked there.
- **Peripheral clocks are whatever the model says.** A UART divisor exactly right in QEMU was five times off on hardware.
- **Memory size**: early page tables covering 4 GiB were enough for QEMU and not for a 16 GiB board.

When touching early boot, MMU or cache setup, linker scripts, load addresses or any lock reachable before userspace, ask what QEMU *emulates away*. Treat green QEMU runs as evidence about logic, never about hardware; get serial output from real silicon before believing a bring-up path works. When a board is silent, try a known-good vendor kernel early — it validates the cable, connector, baud rate, card and firmware in one boot. See [Raspberry Pi 5](../hardware/raspberry-pi-5.md).

## Measure before building

Plans are hypotheses. Several were refuted by measurement before or during implementation, saving the work:

- **Per-epoll-instance wait channels** were planned to kill a thundering herd on the global poll wait channel. After the 512-slot rescans were bounded, the wake path measured 0.7 % — the cost had been the rescan, not the wakeups. The refactor would have touched every readiness publisher for about 1 % and risked lost-wake races. It was not built ([event loop](../kernel/event-loop.md)).
- **"`RUN_QUEUE` is held more than half the time"** measured 8.2 %, with sub-microsecond holds.
- **COSMIC's sluggishness was the kernel, not the software renderer**: an uncached LAPIC read on every `current_pid()`, a 512-slot epoll walk, a 256-slot `tgid_of` scan under the scheduler lock — 0.20 → 1.00 fps once fixed.

Write the prediction down before running the experiment, and state a refuted premise plainly, with the numbers: REFUTED is a valid outcome of a piece of work.

## Absolute versus relative

A recurring class of bug: a value that is absolute read as relative, or the reverse.

- `FUTEX_WAIT_BITSET`'s timespec is an absolute `CLOCK_MONOTONIC` deadline; reading it as an interval made every Rust and relibc timed wait last uptime + timeout — the root cause of the greeter-to-session hand-off "GL deadlock" and of several "slow after boot" symptoms.
- `timerfd_settime` ignored `TFD_TIMER_ABSTIME`.
- The tick handler reloaded the timer from *now*, so interrupt latency stretched every period: a tick counter is not a clock.

When a timing symptom grows with uptime, or "works right after boot and degrades later", check this class first.

## Stale baselines

A fix to a measurement substrate invalidates every number taken on it. All wall-time figures recorded before 2026-09-16 were measured on a clock running 5–36 % slow, and several "slow" symptoms of the same period were collateral of the futex bug; such numbers must be re-measured before being quoted. Lane and wave notes date every figure for this reason.

## See also

- [Debugging](debugging.md) — the instruments themselves.
- [Testing](testing.md) — showing a guard failing first.
- [Design principles](../overview/design-principles.md) — the rules these lessons produced.
- [History](../overview/history.md)
