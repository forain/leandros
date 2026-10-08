# Lane multiterm — 2026-09-27

Symptom (user, Mac aarch64/HVF, gles31 QEMU, wgpu): in COSMIC the first cosmic-term opens; a second launch (Super+T) forks but no window ever appears.

Branch `lane/multiterm` (origin), commits `060c20ab`, `88642455`, and a lockwatch commit on top.

## Verdict: (b) — the 2nd process dies at startup on a full GLOBAL table
- (a) REFUTED from source. The shipped cosmic-term is built `--no-default-features --features wayland,wgpu`, and `main.rs` calls `cosmic::app::run`, not `run_single_instance`. No D-Bus is involved.
- (c) REFUTED. The 2nd instance never created a window.
- Instrument: `/bin/cosmic-term` was replaced in-guest by a wrapper that logs start/exit and appends stderr to `/tmp/ct.$$.err`. The 2nd instance printed `calloop channel.rs:178 "Failed to create a Ping.: Os { code: 24, TooManyOpenFiles }"` from `SctkEventLoop::new`, then exited 0 (after daemonizing).
- The vfs eventfd pool is **system-wide**, fixed at 256. Fill: 53 at the greeter, 235 on an empty desktop, 254 after one terminal (~17 per cosmic-term: tokio + calloop + wgpu). No leak: the slots track live fds.

## Each fix exposed the next ceiling (measured)
1. eventfd 256 → the 3rd terminal hit `[SCHED] task table FULL: 256/256` (~200 threads on an empty desktop, ~19 per terminal).
2. tasks 256 → the 4th terminal failed because cosmic-comp's own **per-process** vfs fd table was full: 128/128 (~100 at idle, +8 per client window).
3. With 5 terminals: tmpfs/memfd 120/128, epoll instances 177/256. ptys 8 cap a desktop at ~7 terminals.

## Changes
| limit | before | after | notes |
|---|---|---|---|
| vfs eventfds (global) | 256 | 1024 | |
| vfs fds per process | 128 | 256 | the ceiling below `SOCK_FD_BASE` (asserted) |
| sched tasks | 256 | 512 | run-queue slot idx packed in 16 bits; pid hint and pid→tgid tables 2048 |
| vfs tmpfs/memfd pool | 128 | 512 | u128 masks → `TmpMask` bitmap; `link_to` biased by 1 so empty = zero (.bss) |
| epoll instances / fds | 256 / 512 | 1024 / 2048 | fd slot u16; empty interest all-zero; range asserted < 0x1000 |
| ptys | 8 | 64 | `/dev/pts` names stay 2 digits (asserted < 100) |

- A full global pool returns **ENFILE** (eventfd, timerfd, pipe, epoll). tmpfs and ptmx return ENOSPC, as Linux does. Each prints one serial line the first time.
- The Ctrl-T dump adds: `eventfds=N/1024 timerfds=N/256`, `heavy:` per-process vfs fd tables (≥24 fds, with an eventfd count), and `[EPOLL] instances=N/1024 fds=N/2048`.
- `TrackedMutex<T, const ID: u8 = 0>` + `new_typed()`: the lock id lives in the type, so a zero payload stays in .bss. `EPOLL_INSTANCES` was putting 16 MB of zeros in the kernel image. aarch64 `.data` went from 28 MB to 11.6 MB.

## Evidence
- Before (main 2f4214bd): 3× Super+T gave exactly 1 cosmic-term on aarch64/HVF virgl AND x86_64/KVM virgl.
- 3 terminals (main fix), both arches, virgl: 3 open, typing lands in each (distinct `$$`), closed oldest-first then the rest: 3→2→1→0. Pools returned to baseline (tasks 211→212, epoll 122→122).
- **8 terminals, final build, aarch64/HVF virgl**: 8 distinct pids typed into, closed oldest first then newest→oldest, each exactly once, 8→0.
  - tasks 203 → 355 → 204
  - eventfds 237 → 389 → 245
  - epoll 122 → 210 → 122
  - tmpfs 90 → 131 → 90
  - fds 637 → 1133 → 644
- **8 terminals, final build, x86_64/KVM Venus**: screenshot shows 8 windows, then an empty desktop after closing.
  - eventfds 195 → 323 → 195
  - fds 624 → 1160 → 624
  - epoll 120 → 208 → 120
  - tmpfs 97 → 187 → 98
  - The harness's `/proc` listing read 0 here: Venus prints a `[DRM] GET_CAPS` line per context on serial, which drowned the reply. Pools and screenshots are the evidence.
- no-GPU suites: see the bottom of this note.

## Open
- cosmic-comp keeps ~1 eventfd/fd per closed client on virgl (43→50 after 8 terminals). It returns to baseline on Venus, so this is likely compositor or GL-driver retention, not ours.
- Venus `[DRM] GET_CAPS capset=4` is printed per context with no rate limit; it floods serial.
- Remaining fixed tables worth watching: pipes (64/128 at idle), timerfds 85/256 with 8 terminals, vfs MAX_PROCS=64 fd tables (46 used with 8 terminals!), net MAX_PROCS=64 socket tables.
- Shared files touched: sched (runqlock/polltimer overlap), vfs, tty pty, kernel epoll.

## --no-gpu suites (release, final build; run as root via fv/suites.sh)
- **x86_64/KVM (desktop): 14/14 RC=0.** sigtest, timertest, pthreadtest, polltest, sigtest2, jobtest, smpwaketest, memtest, forktest, exectest, killmt 100/100, drmsmoke failed=0, uptrtest 112/0, vfstest.
- **aarch64/HVF (Mac): 13/13 RC=0, plus pthreadtest PASS in the guest.** pthreadtest's 5 subtests all printed PASS and the guest printed `RC=0`, but the host `driver.py cmd` never saw the prompt. I killed that one driver process (my own) after 244 s, so the harness row reads `RC=?`. This is a harness hang, not a test failure.
- The `[SYSCALL] sys_execve failed ... 0x..D8` line in exectest is its expected negative (ENOEXEC) case.

Merged to main as fc3e9987 (by the orchestrator). Follow-ups (vfs MAX_PROCS=64, Venus GET_CAPS spam) are handed to lane procpool.
