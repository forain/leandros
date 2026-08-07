# K2 Event-Loop Blocking — progress checkpoint

## Symbol map (post K1-C, re-located)
- sched/src/lib.rs: block_on_port_{prepare:773,cancel:780,commit:789}; deliver_signal:319; deliver_signal_process:358; unblock_port:1126; timer_tick_irq:1007; TICK_HOOK:998; register_tick_hook:1003; ticks:315; exit:1378 (clear_exe when pid==tgid); exit_group:1367
- sched/src/runqueue.rs: block_on_port:153; unblock_port:167; MAX_TASKS:19
- sched/src/clone.rs: fork_current:41; clone_thread:303 (child build ~180-220, ~465)
- kernel/src/syscall.rs: sys_poll:2088; sys_ppoll:2135; sys_select:6118; sys_epoll_wait:5871; sys_epoll_ctl:5813; probe_fd_events:5996; probe_fd_events_seq:6010; poll_fd_state:5958; caps MAX_EPOLL_INSTANCES:5652 INTERESTS:5653 FDS:5695; EPOLL_FD_BASE:5739; dispatch SIGNALFD4:1146 INOTIFY:1241; sys_readlinkat:4903 (/proc/self/exe:4916); sys_execve:2660 (replace_address_space:3006 = success point, kpath in scope); interrupted:1716; imports:15
- servers/net/src/lib.rs: UnixConn:213; handle_poll:1998; handle_send:1155; handle_recv:1294; handle_sendmsg:1469; handle_recvmsg:1595; handle_connect:978; handle_accept:846; handle_close:1920; handle_close_all:2085; NET_POLL:35; SOCK_FD_BASE:0x100 SOCK_FD_END:0x300; val_reply:113
- servers/vfs/src/lib.rs: PipeRing:755 (seq already bumped on write:2703 read:2614!); handle_poll:4165; handle_read pipe:2596 eventfd:2645 timerfd:2657; handle_write pipe:2692 eventfd:2762; TimerFdEntry:872; timerfd_poll_expirations:896; VnodeKind:537; gen_proc_self_content:2177; VFS opcodes ~60; poll_reply near 191
- kernel init tick hook: pipewire/src/lib.rs:177 registers tick_pump (audio). MUST generalize to TICK_HOOKS[4], audio keeps slot.

## Key decisions
- Per-interest snapshot in epoll_wait (lock/probe/unlock per interest), NOT whole-array (avoids 16KB stack + 82d0cc3 hazard).
- Level vs edge: EPOLLET → seq-gated; level → fire whenever cur!=0. (current code force-edges all vfs fds — fix.)
- Net: add UnixConn.seq; handle_poll returns (revents, seq, has_seq byte@16). Only UnixConnected has_seq=1; listeners/inet has_seq=0 (level). Preserves tokio-echo listener behaviour.
- ONESHOT: add EpollInterest.armed; disarm on fire; re-arm on ctl MOD/ADD.
- poll_tick uses try_wake_poll (try_lock RUN_QUEUE); defers on contention. Safety-wake const=false.
- pipe seq already bumped; ADD wake_poll on write/read/eventfd-write.

## Status: STARTING kernel edits

## DONE so far (resumed after stall)
- sched/lib.rs: POLL_WAIT_CHANNEL+block_on_poll_*+wake_poll+try_wake_poll+NEXT_POLL_DEADLINE+register_poll_deadline+EXE_PATHS table(set/exe/clear/inherit); TICK_HOOK->TICK_HOOKS[4]; deliver_signal & deliver_signal_process call wake_poll; exit() clear_exe_path when pid==tgid.
- sched/clone.rs: inherit_exe_path in fork_current + clone_thread(non-THREAD).
- vfs/lib.rs: pipe write/read + eventfd write now drop lock then wake_poll; timerfd settime publishes deadline+wake; earliest_timerfd_deadline() added; VnodeKind::SignalFd{mask}/Inotify{next_wd}; opcodes VFS_SIGNALFD_CREATE=0x4A INOTIFY_CREATE=0x4B INOTIFY_ADD=0x4C; handlers handle_signalfd_create/inotify_create/inotify_add; read+poll+fstat arms added; dispatch wired.
- vfs comm/cmdline from exe: DEFERRED (SHOULD not MUST) — deviation noted.
- userland subagent (aeec08060a741e2ab) writing idletest+epolltest crates — pending.

## TODO
- net: UnixConn.seq + edge bumps/wakes + handle_poll returns seq(has_seq@16) + listener sock_id filter fix.
- kernel syscall: epoll_wait rewrite (per-interest snapshot, level/edge, ONESHOT armed), poll/ppoll/select block tails, probe_fd_events_seq net seq, caps 64/512/128, dispatch signalfd4/inotify, readlinkat exe, execve set_exe_path, thin wrappers.
- kernel init: register poll_tick hook (poll_tick uses NEXT_POLL_DEADLINE+vfs::earliest_timerfd_deadline+try_wake_poll; safety const=false).
- wire userland crates: workspace member, build-userland RELIBC_LINKED, mkfs bins + tokio pack.
- build-all + both-arch QEMU + idle CPU measure.

## net DONE
- UnixConn.seq added+init; net_poll_reply(revents,seq:Option) with has_seq@16; handle_poll returns Some(seq) only for UnixConnected, None else; listener filter now matches bound sock_id (K1-C #1 fixed).
- seq bump + wake_poll (after dropping UNIX_CONNS/SOCK_TABLES) in: handle_send, handle_recv, handle_sendmsg, handle_recvmsg, handle_connect, handle_accept, handle_shutdown, handle_close(UnixConnected), handle_close_all.
- userland subagent DONE: idletest+epolltest written at userland/{idletest,epolltest}. Need workspace wiring + review. Subagent flagged tests target the fixed behavior (level-refire, oneshot, signalfd/inotify) — expected.

## KERNEL syscall.rs NEXT (in progress): caps, EpollInterest.armed, probe_seq net, epoll_wait rewrite, poll/ppoll/select block, dispatch, readlinkat exe, execve set_exe, wrappers, poll_block helper.

## KERNEL DONE
- syscall.rs: caps 64/512/128; EpollInterest.armed; epoll_ctl re-arms; probe_fd_events_seq net-seq via has_seq@16 + dropped socket carve-out; sys_epoll_wait full rewrite (per-interest snapshot, EPOLLET vs level, ONESHOT disarm, 3-phase block + epoll_any_ready reprobe); EPOLLET/EPOLLONESHOT consts; poll/ppoll/select use poll_block+poll_any_ready/select_any_ready; poll_deadline_tick hook; dispatch signalfd4/inotify wired; sys_signalfd4/inotify_init1/add_watch/rm_watch wrappers; readlinkat /proc/self/exe via sched::exe_path; execve set_exe_path; AARCH64 INOTIFY_INIT1 360->26 FIX.
- init.rs: register poll_deadline_tick hook.
- userland wired: workspace members epolltest/idletest; build-userland RELIBC_LINKED; mkfs bins + tokio-echo-selftest pack (both linux-musl arches confirmed present).

## NEXT: build-all, fix compile errors, QEMU both arches, idle-CPU measure.

## Build1 (both) OK, images packed with epolltest/idletest/tokio-echo-selftest.
## Added console-read 1-tick poll-block (acceptance #4 idle-at-login): blocking read(fd0) parks on poll channel with now+1 deadline instead of busy-yield; reprobe checks consumable input (evdev/PENDING_INPUT) NOT raw serial_has_data (would tight-spin w/ IRQs masked); nonblocking keeps 32-spin EAGAIN. evdev push_event calls try_wake_poll. Scheduler idles via hlt/wfi so this drops host CPU.
## Build2 (both) running: b1k76qrge.

## RUN PLAN: driver.py login root/root; scmrun.py for scmtest-style output. Tests on f2fs /bin: epolltest, idletest, tokio-echo-selftest, scmtest(19/19), polltest, plus baselines vfstest/forktest/sigtest/memtest/waittest, boot-to-login. Measure host CPU at login prompt before/after.

## AARCH64 RESULTS (build2, UEFI boot)
- boot-to-login: PASS (console-read change didn't break boot)
- epolltest: 8/8 PASS (et_fires_once, level_refires, et_refires_after_edge, timeout_accuracy, oneshot, signalfd_signo, inotify_never_fires, proc_self_exe got /bin/epolltest)
- idletest: IDLE_CPU_US 0, idle_cpu PASS, timer_wake PASS (safety-wake OFF) — M1 idle criterion MET
- tokio-echo-selftest: pass=3 fail=0 skip=1 (UDS 400 echoes/4 clients, TIME, MPSC)
- scmtest: 19/19
- polltest: 6/6
- sigtest: 6/6; forktest: 3/3; memtest: 4/4; waittest: 4/4 (incl wait_on_process_group flake)
- vfstest: 31 PASS / 3 FAIL (chroot_confines_symlink_resolution, xattr_list_tmpfs, xattr_list_f2fs) — all K2-UNRELATED (chroot/xattr code untouched); documented non-K2 quirks + dirty-image residue from repeated runs.
- idle host CPU at login: ~170% (dominated by pre-existing net_daemon busy-yield, out of K2 scope); need before/after A/B.

## NEXT: x86_64 full run; then #4 CPU A/B; final clean-image vfstest.

## X86_64 RESULTS (build2, UEFI boot) — all K2 green
- boot-to-login PASS; epolltest 8/8; idletest IDLE_CPU_US 0 (2/2); tokio-echo-selftest pass=3 fail=0 skip=1; scmtest 19/19; polltest 6/6; sigtest 6/6; forktest 3/3; memtest 4/4.
- waittest: 1st run wait_on_process_group FLAKE (documented; flake-retry allowed) — retrying.
## REMAINING: waittest retry confirm; #4 CPU A/B (git stash pre-K2 vs K2 login CPU); final clean-image vfstest; commit.

## COMMITTED to branch k2-event-loop (main=pre-K2 3a3120a):
- 0aefc36 sched; db4c75e servers; 2c924ea kernel; 1887047 userland
## x86_64 waittest wait_on_process_group FAILED both runs (passed aarch64) — arch/timing flake per docs; disambiguating via pre-K2 A/B.
## A/B in progress: on main (pre-K2), building x86_64 (bid2tf8q6). Will measure login CPU + waittest + vfstest, then checkout k2-event-loop + rebuild for after.

## PRE-K2 BASELINE (main, x86_64, FRESH image):
- login-prompt host CPU: ~300% (248,297,297,299,300,301) — busy-poll baseline
- waittest: 4/4 PASS (wait_on_process_group PASS)
- vfstest: 34/34 PASS, 0 FAIL
=> The 3 vfstest fails + waittest fail seen earlier under K2 were DIRTY-IMAGE residue from repeated runs, NOT K2 regressions. Must confirm K2 fresh-image = 34/34 + 4/4.
## K2 AFTER build running (b5h4fixn8): checkout k2-event-loop, build x86_64 fresh. Then boot, measure login CPU + waittest + vfstest on fresh image.

## FINAL (all resolved)
- K2 x86_64 fresh: login CPU ~200% (vs pre-K2 ~300%); vfstest 34/34; waittest wait_on_process_group 5-run = FAIL,PASS,PASS,PASS,PASS (4/5) => documented flake, NOT K2 regression.
- Both arches: epolltest 8/8, idletest IDLE_CPU_US 0, tokio-echo pass=3/skip=1, scmtest 19/19, polltest 6/6, sigtest 6/6, forktest 3/3, memtest 4/4, boot-to-login.
- Committed 4 commits on branch k2-event-loop (main=pre-K2 3a3120a). DONE.
