# lane/execleak — 2026-09-24

Branch `lane/execleak` @ `71e95b3` (base `fc5a6bc`). Mac: aarch64/HVF, x86_64/TCG.

## Verdict: both premises REFUTED as kernel leaks. They were a measurement window, now closed in the kernel

Instrument: the buddy free-page count from the Ctrl-T census (`[TASKS] buddy free_pages=`), read
before and after each run with the greeter killed. A temporary buddy leak tracer supported it:
Ctrl-B armed it, and the next Ctrl-B dumped every block still outstanding, grouped by
frame-pointer call chain and TTBR0. The tracer is not committed. Its source is saved as
`lane-execleak-leaktrace.rs.txt`, and it must disable IRQs around its lock; the first version
deadlocked cpu0 against an IRQ-context allocation.

Baseline `fc5a6bc`, aarch64:

| run | in-test reading | census delta (pages) |
|---|---|---|
| killmt 200 exec_worker | −8828 KiB (FAIL) | 4 |
| killmt 200 exec_plain (new) | −36 KiB | 3 |
| killmt 200 fork_exit (new) | −4568 KiB | 0 |
| memtest ×3 | lost_kib_per_death 467–495 | 0–2 per run |

So a plain exec and a multithreaded takeover exec both leak nothing. The "order-5 blocks 601→374"
reading was fragmentation. The in-test losses (about one child, 8–15 MiB) came from `wait_scan`:
it reports a Zombie as soon as `exit()` marks it, but the Task, and with it the address space,
was dropped later by the reaping CPU. That address space holds the eager copy of the 8 MiB user
stack, the writable data, and the page tables. So `sysinfo` read right after `wait4` still
counted the dead child.

## Changes
- `sched/src/lib.rs`: `release_exiting_address_space` runs in `exit()` after the
  clear_child_tid write and before the Zombie mark. It runs only when the caller is the last
  task of its tgid and holds the only Arc. Under RUN_QUEUE it detaches TTBR0/CR3 and sets
  `page_table = 0`, then drops the address space after `busy` clears (the same pattern as
  `replace_address_space`). This matches Linux, where exit_mm runs before exit_notify.
- `sched/src/lib.rs`: the dispatch-loop reap and the in-place group-kill reap call
  `clear_exe_path` for leaders. A leader reaped by a sibling's group kill kept its slot. After
  64 such deaths every exec read `/proc/self/exe` = /bin/init. This was a pre-existing bug:
  baseline `killmt 50` (all modes) fails `exec_worker` with E6.
- `kernel/src/syscall.rs`: `Box<AddressSpace>` leaked about 56 B per exec, because
  `replace_address_space` never returns. The fix unboxes it in an inner scope. The tracer showed
  this as slab refills from `sys_execve`.
- killmt: new `exec_plain` and `fork_exit` modes. It now samples freeram after it settles; a
  signal-killed group can still become waitable before a sibling's CPU drops the last Arc.
  memtest: budget cut from 512 to 256 KiB per death.

## After, 71e95b3
- aarch64/HVF: memtest 6/6 with `lost_kib_per_death=0`, 3 boots. `killmt 50` 11/11, every mode
  at mem +0 (touch −4 KiB). `killmt 200 exec_worker` +0. Census over the whole battery: 6 pages,
  and 0 on a second battery. `drmsmoke --leak 10` ×2: 0 KiB. drmsmoke failed=0, vfstest,
  sigtest2 and timertest pass. The greeter paints. 0 panic and 0 `[WDOG]`.
- x86_64/TCG: memtest 0 KiB per death. `killmt 20` 11/11, all at +0. `killmt 50 exec_worker`
  +0. Census delta for exec modes is 2–3 pages per 50. `drmsmoke --leak 5` reads 0 on repeat
  runs (see below).

## Open / notes
- `[PF] handle_user_page_fault returned false` prints twice per segv iteration. Baseline does the
  same, so it is pre-existing.
- On x86 TCG, the first `drmsmoke --leak` after a greeter kill read 1645 KiB/death: a one-time
  8 MiB of first-use allocation, with 0 on repeat runs. Not bisected against baseline.
- A group killed by a signal still becomes waitable when its leader is reaped, before a sibling's
  CPU drops the last Arc. killmt now tolerates that window; the kernel still has it.
- `with_task_address_space` (evdev/drm) reads a root without a reference, which is a
  pre-existing race against any AS drop.
- Build note: the Mac needs `SDKROOT=.../MacOSX26.5.sdk`, because the SDK 27 tbd files break the
  host linker.
