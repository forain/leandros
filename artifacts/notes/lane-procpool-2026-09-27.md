# Lane procpool — 2026-09-27

Branch `lane/procpool` (origin), base `fc3e9987`. Commits:
- `f65e3695` vfs, net, tty, kernel: size per-process tables to the 512-task limit (+ DRM GET_CAPS once per request)
- `0d13d759` fork: release the tables a child took if it never gets enqueued
- `fc43874a` sched: 1024 tasks; per-process tables sized to a 512-process limit
- `29ffaeda` exec: back images by inode when EXEC_FILES is full

Note: `0d13d759` alone does not build (it already names `MAX_PROCESSES`, which `fc43874a` defines). Both were pushed together. No history rewrite.

## Verdict
- **x86_64/KVM Venus: FIXED.** 20 cosmic-terms open, each typed into, all closed, and every pool returns to baseline.
- **aarch64/HVF virgl: PARTIAL.** 19 terminals work. The 20th opens a blank window (3 of 4 runs) or dies (1 of 4). No table reports full. This is open, see below.

## Changes
| table | before | after | notes |
|---|---|---|---|
| vfs `FD_TABLES` | 64 | 512 (`MAX_PROCESSES`) | now in .bss (`new_typed`; `VnodeKind::None` is zero, only the lock id had kept it in .data), 5 MiB. Scans stop at a monotonic high-water mark. A full pool gives ENFILE (was ENOMEM/EMFILE/EBADF). fork and vfork check for a free table first and fail with EAGAIN. Before, the child silently got no table. |
| net `SOCK_TABLES` | 64 | 512 | Tables are heap-allocated on first use and then reused. This removed 2 MiB from .data; 512 static tables would have been 16 MiB. `MAX_SOCKS` 512 → 510, so one table fits a single order-3 block. A full pool gives ENFILE. Uses named iterator types, because an `impl Iterator` breaks the `match tbls.iter().find()` borrows. |
| tty `CONSOLE_TERMIOS`, `TIMER_TABLES` | 64 | 512 | Scans use a high-water mark, since `check_timers` runs on every syscall return. When full, one serial line each (they used to fail silently with ENOTTY/ENOMEM). |
| kernel `STDIO_FLAGS` | 64 | 512 | When full, one line (F_SETFL used to drop the flag silently). |
| sched `FUTEX_TABLE` | 256 | `MAX_TASKS` | When full it returned EAGAIN, which callers treat as a silent retry spin. |
| drivers `VIRTGPU_CTXS` | 16 | 256 | One per GL/wgpu client, so 16 capped the desktop at about 10 terminals. When full, one line. |
| sched `MAX_TASKS` | 512 | 1024 | On Venus a cosmic-term runs about 18 threads on top of about 220 for the session, so the table filled at 16 terminals. The pid hint and pid→tgid tables grow 2048 → 4096. The new `MAX_PROCESSES` = 512 sizes the per-process tables; `EXE_PATHS` uses it too. |
| kernel `EXEC_FILES` (64) | eager-load fallback | inode-keyed fallback | With 20 terminals about 70 processes are live and the table fills. New execs now back the image through `MMAP_FILES`, keyed by (mount, ino) and pinned, with no f2fs open-file slot held. Before, the whole image was read into anonymous memory per process. It is not raised because each slot pins one of f2fs's 256 open files. |
| DRM `GET_CAPS` log | one line per context | one line per distinct (capset, ver) per boot | 8 terminals on virgl used to print 64 lines. Now virgl prints 2 and Venus prints 1. |

- Leak found and fixed: when enqueue failed (task table full), the fd table, socket table and exe path already duplicated by `before_enqueue` stayed claimed. It cost 4 tables per 20-terminal run (fdtables 30 → 34 after closing everything). The fork paths now tear them down (`undo_unborn_child`).
- The Ctrl-T dump prints `fdtables=N/512 hwm=N`.

## Evidence (4 GiB guest; 2 GiB on the Mac OOM-kills terminals, see below)
x86_64/KVM Venus, desktop, final build (`29ffaeda`): 20 opened, 20 distinct `$$` typed, all closed (cosmic-term count 0). Values are before → all open → after closing:
- tasks 220 → 580 → 221
- fdtables 30 → 70 → 30
- socket tables 27 → 67 → 27
- eventfds 196 → 516 → 196
- epoll 122 → 342 → 122
- timerfds 53 → 133 → 53
- fds 633 → 1960 → 625
- tmpfiles 101 → 318 → 98

Steps along the way:
- 512 tasks: 16/20 opened, then `[SCHED] task table FULL 512/512`, with the 4-table leak above.
- 1024 tasks without the exec fallback: 20/20, same baselines.

aarch64/HVF virgl, Mac:
- 2 GiB: 20 launched, 10 killed by `MEMORY PRESSURE` (about 57 MiB RSS each), 10 survive.
- 4 GiB with 512 tasks: 16/20, task table FULL.
- 4 GiB final: 19 opened and typed into, all 19 closed exactly once.
  - tasks 207 → 569 → 208
  - fdtables 30 → 68 → 30
  - socket tables 27 → 65 → 27
  - epoll 122 → 331 → 122
  - timerfds 53 → 129 → 53
  - eventfds 240 → 598 → 253 (virgl retention, as multiterm saw)
  - fds 639 → 1815 → 650

No-GPU suites, 14/14 RC=0: aarch64 on the final build. x86_64 on the final build too (also 14/14 on `fc43874a`).

## Open
- **aarch64 virgl, 20th terminal**: its window maps and its brush child starts, but the main thread parks in `ppoll` (0x49, no deadline) inside libc, with the same sp every run. Its two virgl contexts are created. With the window hung and focused, Super+Q cannot close it, so the close phase stalls. No `FULL` line and nothing on host stderr. The exec fallback does not change it. Suspects: a host-side virgl/ANGLE limit (context id 0x1B) or a lost wake on the fd it polls. The next step is a stderr + strace-like capture of that ppoll's fds.
- Guest RAM: 2 GiB fits about 10 terminals on aarch64. That is the practical ceiling now, not the tables.
- `EXIT_LOG` (256-entry ring; thread exits count toward it) can drop an unwaited child's status after more than 256 exits. It is scanned under RUN_QUEUE, so it was not grown here.
- evdev clients (16, LRU eviction), vfs `LOCKS` 64, pipes (64 live at idle, flat across runs; the counter looks suspicious).
- `[DRM] virtgpu context created` still prints once per context (about 2 per terminal), unlike GET_CAPS.
- Shared files: sched (runqueue.rs, lib.rs, futex.rs; overlaps runqlock/polltimer), kernel/syscall.rs (fork, exec), vfs, net, tty, drivers/drm_device_interface.rs.
