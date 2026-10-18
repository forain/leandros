# LeandrOS — TODO

Single source of truth for remaining and future work. Anything finished is deleted
from this file, not marked done — `git log` is the record of what happened.

Reconciled against `main` at `a0868279` (2026-10-18). The dated lane sections come first, newest
first; *Standing context* holds facts later work depends on; *COSMIC desktop: older open items*
holds what is still open from the numbered items of 2026-08; *Housekeeping* is last.

---

## Laptop performance wave (2026-10-17/18)

Merged into `main` (`a0868279`). Per-lane detail is in `artifacts/notes/lane-<name>-2026-10-17.md`,
plus `lane-fbmm-2026-10-16.md` and `lane-ffhang-2026-10-18.md`:
- Boot: the kernel fills the gaps in the bootloader's HHDM before mm init (`fbmm`) and has a `netwatch=S` trial-kernel network watchdog (`netwatch`).
- Laptop hardware: per-CPU P-states (HWP/EIST) and `/proc/cpufreq` (`cpufreq`), software RPS on gen9 and `/proc/i915` (`gtfreq`), an i915 hardware cursor and real-vblank flips (`kmscursor`).
- Wi-Fi: iwlwifi rate control, TX backpressure and ERP (`wifirate`); WMM, HT 20 MHz and RX Block Ack with a host reorder buffer (`wifiht`); smoltcp loss recovery without go-back-N (`wifirx`).
- Network: EPOLLET for TCP (`sshspin`), and for UDP, ICMP and listeners, plus connected UDP, EADDRINUSE, UDP GSO and `sendmmsg`/`recvmmsg` (`udp`).
- Wake herds: targeted GPU fence wakes (`gpuwake`), exact poll keys (`mediabus`), foreign tags with keyed timerfd/syncobj/signal/net wakes (`deskbusy2`), a lock-free signal hint (`sigfast2`) and the epoll ready list (`epollready2`; `epoll.ready=0` restores the old path).
- Accounting and procfs: real utime/stime, `/proc/sysstat`, `/proc/rqhold`, `/proc/wakestat` and `/proc/pcsample`; `/proc/<pid>/comm`, `/proc/<pid>/task`, real `tty_nr`/`tpgid` and argv-backed `cmdline`/`environ`, with `/bin/ps` (`procps`, `procps2`).
- `su` and `sudo`, ported from sudo-rs (`sudo`).

On the laptop with the screen lit, YouTube plays with RUN_QUEUE at about 90 k acquisitions/s, down from 516 k.

**Open, display and Firefox:**
- **A Wayland client started while the panel is blanked stalls.** cosmic's idle screen-off comes after about 15-20 min. After it, a newly started Firefox sits at about 0 % CPU and makes no network requests. It is the same on `63bcebe2` and `b6e6f363`. Every thread is in a normal idle wait: the client waits for frame callbacks, which cosmic-comp does not send while the output is off. This needs a policy decision: should a session started over SSH wake the display, or hold an idle inhibitor? The screen cannot be woken remotely today. There is no input injection, and an evdev write only records LEDs. Until this is decided, start laptop A/B runs within about 15 min of a login and compare kernels only under the same screen state (`lane-ffhang`).
- **Web Content spin on the `epoll.ready=0` boot.** This was after the screen blanked, 4.4 h into the boot. Four Web Content threads spun at about 2.8-3.4 cores, almost all in user mode: IPC I/O Child, TaskController, MediaSupervisor and MediaDecoderStateMachine. Together they made about 5 k futex calls/s, with no epoll storm and no playback. It was not reproduced, and it is untested with the screen lit. The fit is a media pipeline left without a presenting compositor, but that is unverified.
- **The desktop is still busy during YouTube**: about 310 % total CPU (firefox ~96 %, cosmic-comp ~55 %, Web Content ~55 %). Firefox decodes video in software, because `ports/firefox/leandros-prefs.js` turns hardware decoding and VA-API off.

**Open, scheduler, epoll and wakes:**
- **Per-CPU run queues: `lane/percpu` is in progress.** Phases 0-2 are running; 3a, 3b and 4 come later. deskbusy2 measured the pick/dispatch/charge path as the largest remaining RUN_QUEUE spin, about 0.43 CPU.
- **The epoll ready list has no laptop A/B.** The plan is a laptop boot with and without `epoll.ready=0`, during YouTube, comparing cosmic-comp's epoll cost against deskbusy2's 613 µs/call baseline. That number cannot be taken today: `/proc/sysstat` is a cumulative on-CPU census per (thread group, syscall), with no per-call epoll_wait timing, and a thread that never returns from epoll_wait counts no calls.
- **relibc's `poll()` misses the ready-list cache on every call.** It creates a fresh epoll instance per call, so it misses every time (11 % of desktop calls). A per-thread cache keyed by the fd set would fix it.
- **Wake producers that still broadcast or skip refinement:**
  - evdev `wake_pollers()` (`servers/evdev/src/lib.rs`): up to 100 herds/s on touchpad motion.
  - The net daemon's 100 ms broadcast safety net: about 190 tasks per broadcast while streaming.
  - Inet listeners report no tag, so sshd parks with a broadcast mask.
  - The deadline-tick service runs on every CPU's tick.
  - A refined keyed wake still dereferences each mask-matching Task: about 11 µs per scan. Mirroring `kbits` and a fingerprint bloom per run-queue slot would bring that to about 1 µs.
- **Producer audit gaps (`lane-epollready`).** Under the ready list, a readiness change with no wake now stalls instead of being rescued by a full re-probe.
  - `TFD_TIMER_CANCEL_ON_SET` is accepted and ignored.
  - Closing an AF_UNIX listener leaves pending connectors without HUP.
- **New writers of signal state must call `sig_kick`.** This covers `signal_pending`, `signal_mask`, `shared_signal_pending`, `stop_pending`, `saved_sigmask` and `group_exit_owner` (use `sig_kick_group` for leader-held fields). A missing kick shows as `miss` > 0 in `/proc/rqhold`'s `# sigfast` line.
- **The switch-back CPU charge still takes RUN_QUEUE.** It was about 22.5 k holds/s, now merged with the next pick. Moving it off the lock needs a lock-free `on_cpu` release protocol.
- **`epolltest unrelated_unix_traffic_no_wake` flakes inside a busy live session**: 11 % against the 1/8 threshold.

**Open, procfs and accounting:**
- **`/proc/<pid>/task/<tid>/stat` field 4 (ppid) names the creating thread.** Linux reports the process's parent for every thread. The cause is that procps2 reuses `proc_stat_of(tid)`, which returns that task's own `ppid`.
- **The `/proc/sysstat` `w` lines exist only as an uncommitted diff.** They give one line per live task (`w pid tgid state sc port listen poll_mask`) and show where every thread of a hung process waits. It was written for ffhang (`ffhang-sysstat-w-lines.diff` in that session's scratchpad) and is worth landing.
- **procps2 limits:**
  - `ps -a`/`a` mean "all".
  - `st_rdev` of `/dev/pts/N`, `/dev/ttyN` and `/dev/console` is 0.
  - A thread's starttime is its group's; cutime/cstime and minflt are 0.
  - `PR_SET_MM_ARG_*` is not implemented.
- **`/dev/tty` without a controlling terminal still proxies stdin.** sudo-rs carries a patch for it.
- **`su leandro -c 'firefox … &'`, backgrounding inside su, is untested.**

**Open, Wi-Fi and network:**
- **The laptop link is still poor.** It shows about 27 % AP retries at -74 dBm ('Anvara'), with HT RX mostly at MCS 3-4. On a ch 1 BSS at about 1.3 Mbit/s, playback took about 3 min to start. Untried: Anvara 5G, `iwlwifi.txant=b` and `iwlwifi.btcoex=wifi` (the wifirate A/B plan).
- **iwlwifi gaps:**
  - No TX aggregation (ADDBA from us, SCD aggregation queue, BA_NOTIF reclaim).
  - WMM parameter changes in later beacons are not applied.
  - No 40 MHz.
  - STATISTICS_NOTIFICATION is ignored.
  - Antennas are only toggled on failure, never compared.
- **Laptop SSH was slow on one `b6e6f363` boot.** It has not been reproduced since and is untested with the screen lit. Separately, on one `73fbe8e1` boot SSH came up about 5 min after the reboot although DHCP was immediate; that is unexplained.
- **UDP gaps: closed by lane udp2 (2026-10-18, artifacts/notes/lane-udp2-2026-10-18.md).** Left open:
  - Laptop: check that iwlwifi (and r8169/e1000e on the PC) accept multicast frames (01:00:5e): mDNS works in QEMU, where virtio-net receives all multicast; the real drivers program no multicast filter.
  - No `IP_RECVERR` error queue, no `IP_PKTINFO` on `sendmsg`, no path-MTU discovery, no IGMPv3/source-specific joins.
  - `SO_RCVTIMEO`/`SO_SNDTIMEO` are accepted but not implemented (a blocking recv waits forever).
  - Ephemeral ports are handed out lowest-free, so a just-freed port comes straight back (Linux randomizes).

**Open, laptop hardware:**
- **HWP at EPP 128 leaves cores 0/1 at 800 MHz while 75 % busy.** Consider `cpu.perf=1` (EPP 0x40) as the laptop default.
- **i915 KMS limits:**
  - Cursor 64x64 only.
  - Watermarks approximate `skl_compute_wm`.
  - No tearing async flips.
  - No real-vblank `DRM_IOCTL_WAIT_VBLANK`.
  - No `IN_FENCE_FD`.
  - A scanout BO that is not PRIME-shared still needs an 8 MB clflush per flip, now in the `i915-flip` worker.
- **Why Limine left RAM above 4 GiB out of the HHDM is unexplained** (`lane-fbmm`). The kernel now fills the gap itself. The cheap next diagnostic is a photo with `earlyfb.stop=1`. If firmware DMA is still changing the tables, the check may miss later changes. aarch64 has no HHDM walker or exclusion.

## Wi-Fi roaming (lane `netroam`, 2026-10-16)

Listeners with an accept queue on every stack, UDP wildcard servers on every
stack, NetworkManager autoconnect rules with per-BSS back-off, exact causes
for networks that cannot be joined (artifacts/notes/lane-netroam-2026-10-16.md).

- [ ] Laptop: run the notes' test list; copy the `not joinable:` line of
  'LEANDRO2026' — it names the cipher/AKM/MFP setting that blocks it.
- [ ] Depending on that line: PSK-SHA256 (SHA-256 KDF + AES-CMAC EAPOL MIC,
  key descriptor 3) is the cheapest; TKIP group (TKIP GTK + Michael MIC) and
  MFP-required (PMF, with WPA3) stay deferred.
- [ ] NM shim: persist `timestamp_s` on a successful activation, as NM does,
  so "most recently used" survives a reboot (today: this boot only, then the
  legacy network, then signal).
- [ ] Two SYNs for one listener in one daemon poll: the second gets an RST
  (one LISTEN socket per pass). A delayed duplicate SYN of an accepted
  connection can still hit the LISTEN socket (slot order is only kept
  against the accept queue).

## git and sshd (lane `gitssh`, 2026-10-14)

git 2.47 is in the base image, from ports/git. `/usr/sbin/sshd` (userland/sshd, russh)
starts at boot on 0.0.0.0:22. Notes: artifacts/notes/lane-gitssh-2026-10-14.md. Open items:
- [ ] IPv6: sshd's `[::]:22` listener fails with EAFNOSUPPORT until the kernel has AF_INET6.
- [ ] The implicit-console fd 0/1/2 has no "closed" state. After `close(1)`, writes still
  reach the console and `fstat` still reports it; Linux answers EBADF for both. `close`
  itself now succeeds.
- [ ] sshd has no privilege separation: the protocol runs as root in one process.
- [ ] sshd lacks `authorized_keys` options (such lines are skipped), keyboard-interactive,
  TCP/agent/X11 forwarding, MaxStartups-style rate limiting and utmp/lastlog.
- [ ] Password hashes are a single salted SHA-256 pass, and the development passwords are
  reachable over ssh wherever port 22 is (the LAN under vmnet or on hardware).
- [ ] The pty slave chown is best effort, because the kernel does not record pty ownership.

## Claude Code CLI (lane `claudecode`, 2026-10-14)

The Claude Code CLI (2.1.296; a ~250 MB Bun binary linked against musl) runs inside an Alpine root, for root and for regular users, on both arches. `claude-code-install` builds the root and downloads the CLI, pinned by sha512. `/bin/claude` is a set-user-ID launcher (`userland/claude-run`) that mounts proc, `/dev`, `/tmp` and `/home` into the root, chroots, and drops to the caller's ids before exec. It is verified in cosmic-term, on the text VT and in a tmux pty, and HTTPS to api.anthropic.com works. `scripts/claudecode-smoke.sh <arch>` covers both users. Kernel work it needed: preadv/pwritev/preadv2/pwritev2, setfsuid/setfsgid, `/dev/tty` gives ENXIO after a pty hangup, VT glyphs, AF_UNIX path resolution, set-user-ID/set-group-ID exec with AT_SECURE and a real `PR_SET_NO_NEW_PRIVS`, and statx/tmpfs-fstat owners.

Lane `ccfs` (2026-10-14): the CLI's Bash tool, its `!` prefix and its file writes failed ("task output swap refused (tasks dir moved or linked)", "ENOENT ... open"). It writes through `/proc/self/fd/N/<name>` below an `O_PATH` directory fd, which LeandrOS answered with ENOENT. Fixed in the kernel (paths below `/proc/<own pid>/fd/N`, `chdir` to the link, `/proc/<own pid>/fd/N` reopen) and the VFS (tmpfs directory stat and fstat agree on st_ino; root gets ENOTDIR for `/tmp/file/x`). chroottest `procfd_walk*`. Verified end to end as leandro on both arches with `scripts/ccfs-mock-api.py`: Bash ran, Write created a file in new directories, Bash read it back.

**Open:**
- **`/run/user/<uid>` is not mounted into the Alpine root.** The CLI logs "Failed to set up sockets directory /run/user/1000/cc-socks" and turns cross-session messaging off. `claude-run` could bind the caller's runtime directory.
- **Paths below `/proc/self/fd/N` follow the directory's open-time path.** The VFS has no dentries, so a directory renamed after it was opened is not followed, and readlink of a removed directory's fd lacks the " (deleted)" suffix.
- **The `procfd_walk*` cases were not run on a Linux host.** The Linux boxes were unreachable during the lane; the expected results are Linux's documented behaviour.
- **The TUI `!` prefix was not driven directly.** It goes through the same task-output pin as the Bash tool, which was verified.
- **The set-user-ID bit survives writes.** A write by anyone other than the owner should clear S_ISUID (and S_ISGID when group-execute is set), as Linux's `file_remove_privs` does. Only `chown` clears it today.
- **No `nosuid` mount option.** Every mount honours set-user-ID bits, including the chroot alias mounts.
- **There is no separate fsuid.** `setfsuid`/`setfsgid` answer with the euid/egid. A privileged caller that asks for a different fsuid gets nothing.
- **fstat on a pty does not report its owner.**
- **Pending AF_UNIX connections whose client closed before `accept` are dropped.** Linux still hands them to `accept`. The chroottest `chroot_unix_socket` case keeps its clients open to avoid this.
- **`uname` reports release `1.0.0`.** Programs that gate features on the kernel version may guess wrong.
- **VT console:** TAB draws a glyph instead of advancing to the next tab stop, and wide characters (emoji, CJK) take one cell, not two.
- **`claude-run` only appends to the Alpine root's `/etc/passwd` and `/etc/group`.** A changed uid, home or shell of an existing user is never updated.
- **Only 2.1.296 is pinned.** A newer release needs `CLAUDE_CODE_VERSION` and `CLAUDE_CODE_SHA512` set when running `claude-code-install`.
- **The x86_64 VT and cosmic-term runs are not screenshotted** (TCG on the Mac). On x86_64 only the tmux pty path and the network check are verified.

---


## Process limits raised (lane procmax, 2026-10-12)

`MAX_TASKS` went from 1024 to 4096 and `MAX_PROCESSES` from 512 to 2048, for an in-guest parallel `cargo build`. Every task walk now costs the live count, not the table size: the occupancy-bitmap `live_slots`, an authoritative pid index, `min_vruntime` over the runnable hint set, wake scans over a parked-task hint set, and futex hash buckets. On the aarch64/HVF COSMIC desktop (virgl, the same coldstart steps), avg dispatch hold went from 1292 to 632 ns, `pick_next` from 1093 to 482 ns, `FUTEX_WAKE` from 3279 to 422 ns, and the deadline tick from 7321 to 2642 ns; total `RUN_QUEUE` hold time fell 31%. Other changes:
- `EXIT_LOG` never overwrites an unwaited process record. Before, 400 children exiting before `wait4` gave 314 `ECHILD`s.
- fork answers `EAGAIN` at the limits.
- `RLIMIT_NPROC` is real.
- `procmax` is in the regress suite.

**Open:**
- **No real `cargo build` has run in the guest yet.** `procmax` covers the limits: ~2030 processes, ~4080 threads, and a burst of 400 exits before `wait4`. It also passes inside a live COSMIC session on aarch64. x86_64 COSMIC was not re-checked on the Mac (TCG).
- **The wake scans still cost the number of parked pollers.** The run-queue park mirror (`pk_port`/`pk_mask`, lane deskbusy2) lets `unblock_port_tagged` skip non-matching parked slots without dereferencing their Tasks, but the scan is still O(parked), and every mask match is still dereferenced. A per-tag index would make it O(woken).
- **`RLIMIT_NPROC` costs non-root callers an O(live) walk per fork.** It counts the caller's tasks by real uid. A per-uid counter would remove the walk.
- **The pid index is authoritative.** Any new in-place change of `Task::pid` must call `RunQueue::reindex`, or lookups of that pid miss.
- **Behaviour change:** a parent whose `SIGCHLD` is `SIG_IGN` or has `SA_NOCLDWAIT` now has its children auto-reaped, as on Linux, so its `wait4` gets `ECHILD`.

---

## Syscalls: splice, rseq, pidfd (lane `syscalls`, 2026-10-12)

`splice`/`tee`/`vmsplice`, `rseq` and the pidfd family (`pidfd_open`, `pidfd_send_signal`,
`pidfd_getfd`, `waitid(P_PIDFD)`, `clone(CLONE_PIDFD)`) replaced their `ENOSYS` stubs; semantics
in `docs/kernel/processes.md#pidfds`, `docs/kernel/threads-and-futex.md` (rseq) and
`docs/servers/vfs.md` (splice); `syscalltest` is in the regress suite. tokio (brush) now reaps
children through pidfds instead of `SIGCHLD`. Still open:
- **`syscalltest` has not been run against a host Linux yet** (it is C for exactly that: `cc
  syscalltest.c -lpthread` on the linux box, then diff the PASS/FAIL lines).
- `clone(CLONE_PIDFD)` installs the pidfd after the fork; if the caller's fd table is full at
  that point the child exists and `parent_tid` reads -1 (Linux fails the whole clone instead).
- rseq ignores `RSEQ_CS_FLAG_*` in a descriptor and reports the CPU number as `mm_cid`.
- splice copies through a 16 KiB kernel buffer (no page moving); a writer racing into the same
  pipe between `pipe_space` and `pipe_append` can make a socket or console source lose bytes
  (files are rewound).
- `pidfd_getfd`'s permission check is "same real uid, or root", not ptrace-attach.

---

## Descriptor limits (lane fdmax, 2026-10-12)

Done on `lane/fdmax`: one per-process descriptor namespace (sockets and epoll fds are fd-table
entries naming internal handles), growable chunked fd tables keyed by tgid, RLIMIT_NOFILE enforced
(soft 1024 / hard 524288 / nr_open 1 Mi), the global pools (pipes, eventfd, timerfd, ofd, epoll,
locks, device opens, f2fs open files, mmap inodes) grown on demand, `/proc/self/fd` listing,
`/proc/sys/fs/{nr_open,file-max,file-nr}`; `fdmaxtest` in the regress suite. Still open:
- **Global scans on close**: the last close of a tmpfs or mounted file still scans every process's table to decide whether the object is still referenced — O(all open fds). Since lane fdgaps the scan takes `FD_COPY_GATE` exclusive (fork and SCM_RIGHTS copies wait for it); a per-object refcount would retire both the scan and the gate.
- **TMP_FILES (4096 slots) and the AF_UNIX connection pool (512)** are still fixed system-wide pools.

---

## Open work (2026-10-11 reconciliation)

Reconciled against `main` (`25075a0`) after the real-PC bring-up lanes of 2026-10-07 to 2026-10-11
(per-lane detail in `artifacts/notes/lane-<name>-2026-10-0{8,9}.md` and `lane-loginhang-2026-10-10.md`;
the user-facing description is the Handbook: `docs/hardware/pc.md`, `docs/servers/filesystems.md`,
`docs/kernel/memory.md`).

**Still open (from this wave):**
- **r8169 RX stalls and chip resets on the PC: fix needs PC confirmation.** Boot 16 (235 s and 245 s of uptime) showed `IntrStatus=00d4` with RxDescUnavail/RxFIFOOver latched and enabled but no interrupt counted: a dead MSI, because the ISR masked only RxOK and acked only what it had read, so a bit latched in between kept the edge-triggered condition asserted. Lane `r8169irq` adopted Linux's protocol (ISR masks all + acks what it read; `rx_rearm` unmasks, re-reads status, re-polls if pending, acks bits outside the mask; overflow bits not enabled on 8168, never a reset), model-tested in `drivers/r8169-tests`; the watchdog now tells a starved daemon (`RX backlog`, no reset) from a stopped chip (soft recovery, then reset). On the next PC boot under load: `grep -E 'r8169\] (stats|RX )'` — expect no `RX stall`/`RX backlog` lines, and note the `irq recheck rescued` totals; send `/proc/net/nic` if anything fires.
- **The PipeWire sink is still named "Virtio Sound Output"** (`ports/pipewire/leandros-snd-sink.c`) whatever the backend; on the PC the Sound panel shows that name for the HDA output.
- **`sigtest` flake**: intermittent failure in the regress suite; not yet root-caused.
- **F2FS roll-forward fsync and NAT/SIT journaling**: fsync is durable only through a full checkpoint (no fsync node log, no NAT/SIT journal in the checkpoint). Lazy fsync (`f2fs.fsync=lazy`, on the NVIDIA stick) trades a ~5 s data-loss window for latency; a real fsync log would remove most of that trade.
- **Coarse dirty-page tracking in the page cache**: with no reverse map a writable PTE cannot be write-protected again, so a dirty `MAP_SHARED` page stays dirty (and is written back whole) while it is mapped writable.
- **Truncate under a live mapping** gives the mapper a detached frame instead of `SIGBUS` on the next access.
- **Page-cache global-lock contention is unmeasured**: `CACHE` is one spin lock (held briefly, never across I/O, never preempted holding it); its cost on many CPUs has not been measured.
- USB hubs remain unsupported (the keyboard dead-on-a-hub class of failure on the PC), and the stick's 0.5-1.3 s write commands are hidden by the fixes above, not removed.

---

## Open work (2026-10-01 reconciliation)

Reconciled against `main` (`6a500e1`) after the 2026-09-27 to 2026-10-01 lanes (per-lane detail in
`artifacts/notes/lane-<name>-2026-10-01.md`).

**Still open:**
- **cosmic-term exit use-after-free in iced's SCTK thread**: upstream; the user chose not to patch (Linux has it too).
- **fd-limit fence loss**: hitting EMFILE in a virgl client's submit still kills its context, as on Linux; fd headroom (`RLIMIT_NOFILE`, soft 1024) is the defence. "Submit then fail the fence" would need a Mesa fence-NULL review.
- **epoll deviations**: no ELOOP for nested epoll cycles, and no `EPOLLEXCLUSIVE` EINVAL checks (the flag is not interpreted at all).
- **`close_range`**: `CLOSE_RANGE_UNSHARE` is accepted and changes nothing, so in a multithreaded caller the siblings see the closes.
- Interrupt-context serial diagnostics can still drop under back-pressure (by design).
- **Worktrees awaiting the user's decision (desktop):** `leandros-applets` (2 unpushed revert commits) and `leandros-applets-k` (uncommitted futex/sched edits).
- **wwoverview left `~/Projects/leandros-wwoverview` (and branch `lane/wwoverview`) on the laptop**; remove it.

---

## Open work (2026-09-24 reconciliation)

Reconciled against `main` following the 2026-09-24 wave (`artifacts/notes/wave-2026-09-24.md`;
per-lane detail in `artifacts/notes/lane-<name>-2026-09-24.md`), plus what was still open from the
2026-09-18 sweep (`artifacts/notes/wave-2026-09-18.md`).

**Still open:**
- **Venus (Vulkan) is infeasible on the Mac with any QEMU today** (`lane/macqemu`/`macgpu`):
  upstream virglrenderer's Venus is Linux-only (epoll/memfd/udmabuf/eventfd); UTM's macOS fork
  fails at host `vkCreateInstance` with an undetermined CS error. The Mac's GPU path is virgl over
  ANGLE/Metal only, via the from-source QEMU at `~/.local/qemu-gpu`.
- **cosmic-session's launch-pad output queue is unbounded** (upstream weakness, not fixable in this
  repo) — contained, not fixed, by an init-side rotating logger (16 MiB cap) plus a
  memory-pressure guard that kills and lets greetd's own respawn/backoff recover.
- **Lazy-fault residual noise, ~27 pages/death** (`lane/greeterleak`'s remaining `vmm.rs` lazy-fault
  residual after the eager-tail fix) — within noise, not chased further.
- **Every exec maps an eager 8 MiB stack** (`lane/procrss`): about 8 MiB of each process's RSS;
  a lazy stack would save ~250 MiB in a COSMIC session. Written private file pages still count as
  RssFile (no dirty tracking).
- **`/proc/kmemstat` has a parse flake** in at least one consuming script — not root-caused.
- **One control-queue chain stays parked host-side per abandoned GPU fence** (`lane/polish`'s
  `ctx_abandon_fences` open item): only reached if the 200 ms drain-before-`CTX_DESTROY` times out;
  QEMU never completes that chain.
- **Panel-embedded applets still render with tiny-skia**: cosmic-panel's embedded server has no
  linux-dmabuf global (`lane/wgpuapps`; the other COSMIC clients run on wgpu). On aarch64/virgl a
  patched-Mesa wgpu stage made iced clients die with `EBADF`, so it is not shipped; root cause open.
  The aarch64 stage's `libvulkan.so.1` fails to relocate `__stack_chk_guard`.
- **Worktree hygiene**: the desktop's `leandros-complk` worktree has 12 uncommitted source files
  that need triage (keep, discard, or land as a proper lane).
- Laptop needs `sudo pacman -S vulkan-intel` if Venus/ANV there is ever found not to work (not
  re-verified against a fresh Venus run on the laptop).
- **P0 #5: aarch64 clicks on the desktop** still need a check with a physical mouse.
- The Mac `~/code/brush` sibling must be synced before building brush: `ports/brush/sync.sh apply
  ~/code/brush` (the `build_brush` sync check fails otherwise).

---

## Standing context

Facts that future work depends on and should not have to re-derive.

**Goal.** Run the COSMIC desktop environment **totally unmodified** (source:
`../cosmic-epoch`) on both x86_64 and aarch64 under QEMU; build-configuration flags
(`--no-default-features`) are allowed.

**The policy, stated exactly, because the previous wording was an absolute that the tree
refuted.** *Temporary* edits to COSMIC source **are allowed, for investigation and discovery
only** — prints, counters, panics, anything that localises a fault. They **must be reverted**
before the work lands, and the revert must be **proven, not asserted**:
`git -C ../cosmic-epoch status --porcelain` empty, submodules likewise (pinned at
`epoch-1.3.0`). A reverted tree with an instrumented binary still staged in the image is worse
than either, so rebuild from clean source before shipping. **Record what the instrumentation
taught you in `artifacts/notes/`** — a finding whose only evidence was a deleted `eprintln!`
is a finding nobody can re-check. **Any permanent fix belongs on the LeandrOS side** — kernel,
shims, or our own launcher — never in COSMIC.

**One permanent COSMIC patch remains**: `ports/cosmic-comp/0001-toplevel-info-weak-window.patch`
(lane `weakwindow`; see *COSMIC desktop: older open items*). The greeter's and the session's
patches are retired. (`ports/busd/current-thread-runtime.patch` is fine — busd is ours.)
Everything beneath COSMIC — kernel, libc, system libraries, daemons — is ours.

**Suite baselines.** On fresh images with `vfstest` run exactly once per image, both
arches: vfstest **36/0**, scmtest **36/0** (`fork_inherits_pending_connector` made it 36), drmsmoke **29/0** (22 → 25 with `edad115`'s console guards, → 29 with `c8cbbc1`'s atomic lane), wakepolltest 10/0,
forktest 3/0, epolltest **10/0** (was 9/0 — `proc_pid_exe` added with the
`/proc/<pid>/exe` fix), polltest 6/0, sigtest 6/0, timertest 6/0, memtest 4/0,
idletest 2/0 (`IDLE_CPU_US 0`), evtest2 8/0, **ptytest 15/0**, **vttest 12/0**, and
**evsplit `BROADCAST 60/60`** (two readers of one evdev node, 60 injected moves; a split
delivery reads 30/30 and is the pre-broadcast signature). `waittest` has **9** subtests since
the pty wave added SIGTERM/SIGKILL/normal-exit/SIGSEGV/SIGILL encoding cases, and is **9/0 on
both arches**. `wait_on_process_group`'s historical flake was the TEST racing `fork` → child
`setpgid(0,0)`+`_exit` against parent `waitpid(-pid)`, not the kernel — fixed in the test
2026-09-16 (`d57ffed`); 9/9 is now the expected result every run, no flake.
Note `waittest` also emits a trailing `WAITTEST: PASS` summary line, which a
`grep -c ': PASS'` miscounts as an extra subtest — read the binary's `failures = N` trailer
instead. On a **Venus host** (the Linux box, `--venus`): `venustest` **108/0 both
arches**, `vktest` 14/0, `vkrender` **51/0** with `s2_checksum = 0x02C0FDC5` pinned across
x86_64/KVM, x86_64/TCG and aarch64/TCG, `vkswap` **21/0** (x86_64). `vkrender` under KVM
does **not** need `VN_PERF=no_fence_feedback`; that dependency died with `18a7a9f`.

**A Mac `venustest` run is worth nothing, in either direction.** QEMU 11.0.2 on macOS has
**no blob-capable virtio-gpu device at all**: `virtio-gpu-pci,blob=on` is refused with
*"need rutabaga or udmabuf for blob resources"*, and neither `virtio-gpu-gl-pci` nor any
rutabaga variant is compiled in. `VIRTIO_GPU_F_RESOURCE_BLOB` is never advertised, so no
blob BO can be created and nothing downstream of one can be exercised. A Mac `venustest`
reports **42 lines, 11 PASS / 31 FAIL**, byte-identical on patched and unpatched kernels.
Do not compare that against the box's numbers and conclude anything. Everything blob-,
HOST3D- or Venus-shaped goes to the box.

**The Linux box.** `forain@172.16.158.150`, `/home/forain/Projects/leandros`, EndeavourOS
(**Arch, not Debian**), virglrenderer 1.3.0, QEMU 11.0.1, Mesa 26.1.3, host GPU a Ryzen 9
7950X iGPU (RADV RAPHAEL_MENDOCINO). aarch64 there needs `-cpu max,lpa2=off` (the Limine
11.4.1 FEAT_LPA2 wedge). Sync Mac → `origin` → `git pull --ff-only` on the box. Compare
two trees by `git patch-id --stable`, not by SHA. The working `--venus` device line is
`virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G -display egl-headless`; `-nographic`
silently overrides `-display`.

**cosmic-comp offers no dmabuf to clients here — measured, not inferred.** A live aarch64
session advertises 54 globals on `/run/user/0/wayland-1`, identical across three passes 30 s
apart; `zwp_linux_dmabuf_v1`, `wl_drm` and `wp_drm_lease_device_v1` are all absent, and no
`wayland-1-card0` socket exists, so `create_socket` (`cosmic-comp/kms/socket.rs:31`) was
never called and `is_software` is true. **Scope:** absent *in this configuration* —
software EGL, forced because the macOS host has no EGL, so `virtio-gpu-gl,venus=on` is
unusable and the guest has no hardware GL. It would flip only if the guest gained a
non-software EGL device. Full report and controls:
`artifacts/notes/m9-crossopen-dmabuf/stage0a-wl-globals.md`.

**`RUST_LOG=trace` cannot read smithay's own damage-tracking decisions.**
`cosmic-comp/Cargo.toml:61-62` sets `release_max_level_info` on `tracing`, so `trace!`
calls are compiled out of the release build and the feature ceiling cannot be raised
additively. Kernel-side counters are the only instrument, and the `FB_DAMAGE_CLIPS` blob
is the damage tracker's **verbatim** output (`PlaneDamageClips::from_damage`, smithay
`backend/drm/surface/mod.rs:68-100`, is a 1:1 `map` with no splitting or merging), which
is what makes the kernel-side decode a real measurement of a client-side decision. It reads
the shaper's output only; the input is not visible from the kernel at all.

**vmnet gotcha.** On a Mac with `socket_vmnet` installed, `driver.py` uses vmnet rather
than slirp, so the guest gets a `192.168.105.x` lease and `10.0.2.x` does not exist —
pings to `10.0.2.2` will silently see nothing. Force `-netdev user` (`LEANDROS_NET=user`) to reproduce the
documented slirp configuration.

**Committed architecture** (settled; revisit only with a reason):

- COSMIC builds for `*-unknown-linux-musl`, **dynamically linked** against a real
  `ld-musl`. No Rust std port. `dlopen` is on the critical path in three places
  (cosmic-comp's EGL loading, Mesa's GBM/DRI loader, cosmic-panel's `use_system_lib`).
- Graphics: **GPU rendering by default** (lane `gpudefault`): `run-qemu.sh` picks Venus
  where the host can do it, else virgl; the Mac runs virgl over ANGLE/Metal. Zink over Venus
  is how cosmic-comp composites on Vulkan, since smithay has no Vulkan renderer. On real
  hardware the drivers are i915 (iris/ANV) and NVIDIA (NVK/Zink). Software rendering is not
  a fallback to aim for. The atomic KMS path is live and preferred. card0's DRM
  identity follows the device — `virtio_gpu` when VIRGL is negotiated so Mesa's loader
  picks a hardware driver, `leandros-drm` otherwise so it falls through to software.
- Seat/input: **shim** libseat and libudev; **port** real libinput and libxkbcommon.
  No seatd, no udevd. **VT switching is implemented** — six VTs, the VT/KD ioctl set and
  Ctrl+Alt+Fn — reversing the original out-of-scope decision. The libseat shim is
  now a *producer* as well as a consumer: it emits
  `SessionEvent::ActivateSession`/`Deactivate` driven by `/dev/tty0`, and DRM master is
  scoped to the VT it was granted on.
- D-Bus: **busd** (pure Rust, from the zbus authors). Reference `dbus-daemon` is the
  fallback if it proves immature.
- Login is graphical: `greetd` (upstream `kennylevinsen/greetd`, pure Rust, ported under
  `ports/`) → `cosmic-greeter` (uid 990) → the user's session, started under **brush**.

**Load-bearing session env** (in the launcher at
`artifacts/m6-session-data/start-cosmic-leandros`):

- `COSMIC_RENDER_DEVICE=226:0` — card0's dev id. Without it, cosmic-comp's
  `determine_primary_gpu` filters our software device out and falls back to an EGL
  display that lacks `EGL_EXT_image_dma_buf_import`.
- `COSMIC_DISABLE_OVERLAY_SCANOUT=1` — **never** `COSMIC_DISABLE_DIRECT_SCANOUT=1`.
  smithay's `FrameFlags::ALLOW_SCANOUT` is a union that includes
  `ALLOW_CURSOR_PLANE_SCANOUT`, so the latter silently disables the cursor plane with
  no log line at any level.
- `SMITHAY_USE_LEGACY` must stay unset so smithay takes the atomic path.

**The PTY contract `cosmic-term` enforces — these are kernel behaviours, not libc
conventions.** It reaches the kernel through `rustix 1.1.4`'s `linux_raw` backend, i.e. **raw
`syscall` instructions, bypassing musl entirely**, so none of this can be absorbed by a libc
shim later. The sequence is `openat("/dev/ptmx", O_RDWR|O_NOCTTY|O_CLOEXEC)` → `grantpt` (a
literal no-op on Linux) → `ioctl(TIOCSPTLCK, &int(0))` → `TIOCGPTPEER`, falling back to
`TIOCGPTN` + `open("/dev/pts/N")` **only on `ENOSYS` or `EPERM`**
(`rustix-openpty-0.2.0/src/lib.rs:243-247`). **So returning the conventionally-correct
`ENOTTY` for an unimplemented ioctl makes every terminal tab fail to open** — the fallback is
never reached, and the correct-looking errno is the bug. Four further hard requirements:
master is set `O_NONBLOCK` with `fcntl(F_SETFL)` wrapped in `assert_eq!(res, 0)`; the reader
is one dedicated thread on **level-triggered** `epoll_pwait`; **`read()` on a master whose
slave is gone must return `EIO`** — `event_loop.rs:282-290` swallows EIO and waits for
SIGCHLD, so returning EOF instead is an unbreakable 100% CPU spin; and resize is `TIOCSWINSZ`
on the **master**, with alacritty never sending `SIGWINCH` itself. Panic traps, each of which
kills the tab outright: `assert_eq!(entry.pw_uid, uid)` (`unix.rs:93`), the `F_SETFL` assert
(`:436`), and `die!` → `exit(1)` on a failed `TIOCSCTTY` (`:55`) or `TIOCSWINSZ` (`:414`). It
**daemonizes by default** (`main.rs:148`) — run it `--no-daemon` or lose stderr.

**Kernel invariants.**

- **`/dev/tty` is per-session, and our serial console is NOT a separate tty.** `/dev/tty`
  resolves through `pty::ctty_for_sid(current_sid())`, so a process under a terminal emulator
  reaches the emulator's slave rather than the machine console; `stat("/dev/tty")` follows the
  same resolution, or `ttyname()` rejects the fd. The second half is the trap: Linux can
  silence a raw-mode VT's keystrokes wholesale because its serial console is a *different*
  tty. Ours is the same one — `arch/*/timer.rs::on_tick` manufactures a synthetic `EV_KEY
  value == 2` on the keyboard node for every UART byte. **Any gate on the console keyboard tap
  must exempt `value == 2`**, or serial input dies the instant a compositor sets
  `KD_GRAPHICS`.
- **Filesystem permissions are enforced on every path operation (lanes `perms`, 2026-09-16,
  and `vfsperm2`).** The gates: traversal (`MAY_EXEC` on every directory component of the resolved path, in
  `servers/f2fs` `resolve_path_ex` and `servers/vfs` `tmp_resolve_links` — EACCES before the
  lookup, so a missing leaf under an unsearchable directory never leaks as ENOENT), creation
  (`open(O_CREAT)`, `mkdir`, `mknod`, `symlink`, `link`, AF_UNIX `bind`: write+search on the
  parent), removal (`unlink`, `rmdir`, `rename` on both parents + the sticky rule → EPERM,
  write on a directory that changes parents), AF_UNIX `connect` (write on the socket inode),
  `O_TRUNC` (write), `chown` (uid root-only, gid only to the caller's egid). All of it is
  one evaluator, `xattr::may_access` (mode bits + stored POSIX ACL + root bypass), so ACLs
  keep working on the new sites. `/tmp` is now `1777` like `/dev/shm`. `userland/permtest`
  pins the matrix on f2fs (`/data`) and tmpfs. The greeter's two dependencies on the old gap
  (reaching `wayland-1` and greetd's control socket) were resolved by staging, not by holes: `userland/init` seeds `/run/user/<uid>` per passwd
  account, the greeter phase lives in `/run/user/990`, and `greeter-launch` chowns the two
  socket nodes to the account before dropping (`ports/greetd/README.md`). Every gate takes an `xattr::Cred` (euid, egid,
  supplementary groups — `setgroups`/`getgroups` are real, `/bin/login` sets them from
  `/etc/group`); `execve` needs x on the ELF, not just on a `#!` script; `utimensat` (and
  x86-64 `utimes`/`utime`/`futimesat`) is real, with the owner-vs-write rule, and both
  filesystems keep atime/mtime/ctime (f2fs inode bytes 36..72) — `stat` used to report 0;
  a directory's default ACL is inherited at creation (`xattr::acl_create`, the umask
  travels with the mode as `mode | umask << 16` and is ignored when a default ACL applies).
- Never touch user memory under `RUN_QUEUE` or any IRQ-off spinlock. Use
  `validate_user_buf`/`read_user_buf`/`write_user_buf`. A re-entrant `RUN_QUEUE`
  deadlock from exactly this froze all four vCPUs once (fixed in `82d0cc3`).
  **Trap for next time** (`26eebf0`, `handle_send`): `read_user_buf` alone does not
  fault a lazy page in — it resolves through `virt_to_phys`, which returns `None`
  instead of faulting, and `sys_sendto` never calls `prefault_user`, only
  `validate_user_buf`. Either pair it with `prefault_user` (private to the syscall
  crate) or hoist the copy above the lock so the fault happens with nothing held.
- **Anything sampled from an IRQ-context hook must use `try_lock` and must report a missed
  sample as missed.** `drm_tick` (`drivers/src/drm_device_interface.rs:1826`) runs at
  100 Hz in IRQ context; a blocking `.lock()` there deadlocks the instant the tick
  interrupts a thread already holding the same mutex from an ioctl — the `RUN_QUEUE` freeze
  shape, which wedges every CPU with no panic. `bo_census` (`:1801`) is the worked example:
  `try_lock` only, and contention yields `u64::MAX` rather than 0, because a zero would be
  indistinguishable from "every object was freed", which is the exact conclusion the
  instrument exists to support or refute.
- **The kernel is softfloat on both arches and must stay that way.** The EL0 trap
  frame saves no vector state, so any kernel code LLVM lowers through a vector
  register lands on the interrupted thread's. Both kernel target JSONs disable the
  vector units; `cpu_switch_to` is the single deliberate exception and scopes the
  extension with `.arch armv8-a+fp+simd` … `.arch armv8-a`. Six items across the previous
  session trace back to this clobber, directly or as the cause that retired them.
- **A borrowed VMO's page list is immutable**, and **an exported dmabuf fd keeps its DRM
  object alive** (`3dbba0c`, `49399f9`). Together these close four hazards measured *live*,
  not theorised: an unpatched kernel returned a valid mapped address for a page past the
  frames the DRM layer lent it, accepted an 8-byte `write()` into it, *succeeded* at
  shrinking a borrowed frame list (order-0 frees out of an order-N buddy block), and let a
  `GEM_CLOSE` recycle memory an open fd still named. The refcount is one per gem handle and
  one per exporting `TmpVmo` slot. **Failure direction, worth knowing before debugging it:**
  the lifetime fix can only make buffers live *longer*, so its failure mode is a leak where
  the previous failure mode was memory corruption.
- **Handle retirement has exactly one path.** `gem_handle_delete(handle, open_id)`
  (`drivers/src/drm_device_interface.rs:3283`) serves both `GEM_CLOSE` and
  `MODE_DESTROY_DUMB`, dropping one reference regardless of which registry minted the
  handle. Gallium's kms-dri winsys releases *every* imported `pipe_resource` through
  `DRM_IOCTL_MODE_DESTROY_DUMB` rather than `GEM_CLOSE`
  (`src/gallium/winsys/sw/kms-dri/kms_dri_sw_winsys.c:288-296`) — upstream's shape, not a
  Mesa bug — so any new retirement route must go through the same function or it will miss
  imports.
- Release builds only — debug builds crash early. Test **both** arches in QEMU after
  every change. Minimum Limine revision is **6**, never downgrade.
- Regression images must be freshly regenerated — run vfstest **exactly once** per
  freshly generated image. A dirty f2fs image produces phantom failures
  (`chroot_confines_symlink_resolution`, `xattr_list_tmpfs`, `xattr_list_f2fs`). The
  historical aarch64 `xattr_list_f2fs` red has not appeared on either machine across two
  sessions, consistent with it being that artifact and not an arch bug.
- **A guard test must be shown to fail with its guard removed, or it is certifying a
  hazard it never checked.** Every guard landed this wave was falsified by mutation with
  control/mutant/restore md5s recorded and the restore byte-identical to the control; that
  is the bar. A test that cannot fail and an instrument that cannot report failure are the
  same defect.
- **Subtest comments and source comments must not cite TODO item numbers.** This file gets
  renumbered as items land — every citation has drifted within a day, every time. Point to
  the defect or the commit instead; those do not move. **No violation is outstanding.**
  `git grep -n 'TODO\.md item\|TODO item [0-9]'` over the source trees returns nothing at
  HEAD; hits under `artifacts/notes/` are dated reports, which are historical records and
  deliberately out of scope. (`userland/vfstest/src/main.rs:51` names an item by *topic*,
  "extended attributes", not by number, and is fine — the rule is about numbers, which
  move.) **Re-run that grep at reconciliation time and paste what it returns.** Twice now
  this file has recorded a violation that an ancestor of HEAD had already fixed, the second
  time while asserting the grep had been run.

**Memory attributes, measured rather than assumed.**

- *aarch64.* `MAIR_EL1` arrives as a flat **`0x00000000000000ff`** under Limine 11.4.1 —
  attribute 0 is `0xFF` (Normal WB/WA) and **attributes 1..7 are all zero**, i.e.
  Device-nGnRnE. So **both** `ATTR_DEV` (index 1) and `ATTR_NOCACHE` (index 3) select
  Device-nGnRnE — not Device-nGnRE and not Normal-NC, whatever their names suggest.
  `a85e209` corrected both comments (`arch/aarch64/src/paging.rs:19` and the doc block at
  `:25-37`) so the source now agrees with the register. `18a7a9f` installs **index 2 =
  `0x44`** (Normal Inner/Outer Non-cacheable) with a read-modify-write in
  `mmu::enable_identity`, before `arch::init` maps anything and before `smp_init` snapshots
  MAIR for the APs, and prints `[ARCH] MAIR_EL1 before=… after=…`
  (`arch/aarch64/src/lib.rs:84`) so the inherited value stays visible. The aarch64
  framebuffer is therefore Device memory and is **deliberately left that way**; it works
  only because `pitch = width*4` keeps every access aligned. Use `ATTR_NORMAL_NC` for
  anything that needs non-cached *Normal* memory.
- *x86_64.* Limine 11.4.1 programs `IA32_PAT` to `0x0000_0105_0007_0406` (PA0 WB, PA1 WT,
  PA2 UC-, PA3 UC, PA4 WP, **PA5 WC**, PA6 UC, PA7 UC) — originally a static decode of a
  `mov ecx,0x277` / `wrmsr` site in `BOOTX64.EFI+0x42f34`, now a **runtime read** confirmed
  identical on two machines by `[ARCH] IA32_PAT before=… after=… wc=…`
  (`arch/x86_64/src/paging.rs:238`). `BOOTAA64.EFI` has zero such sites; only our
  direct-boot path (`kernel/src/entry_x86_64.s`, which writes EFER and nothing else) leaves
  the reset PAT. `5ce8af2` makes every CPU agree: `init_pat_bsp()` is the first statement of
  `arch::init`, `init_pat_ap()` the first statement of `smp::sched_ap_entry`, and **PA5** is
  the guaranteed WC slot (`PAT_WC_READY`), chosen because the write is provably inert on the
  Limine path — PA5 is already `0x01` there, so the read-modify-write is value-identical and
  cannot reinterpret a live translation, including Limine's own PAT-bit framebuffer mapping.
  On direct boot PA5 goes WT → WC and provably has no users, since reaching PA4..PA7
  requires the PAT bit and the 2 MiB PDEs `entry_x86_64.s` builds have bit 12 clear.
  `wc=0` means the CPU or hypervisor refused the write and we fell back to PCD/UC-, which is
  `18a7a9f`'s behaviour and not a failure.
- *The divergence this fixed was live.* Before `5ce8af2`, `arch::init`'s `NO_CACHE` re-map
  of the framebuffer failed on 1536 of 2025 pages because `map_4k` cannot split Limine's
  huge pages, leaving `PD[0] = 0x8000108b` — a 2 MiB leaf selecting PA5, i.e. WC on the BSP
  and WT on every AP, one set of physical lines under two memory types, which the SDM leaves
  undefined. The console writes through Limine's HHDM mapping
  (`drivers/src/framebuffer.rs:653`). It never bit us because WT is coherent and the console
  is idempotent.
- *WC is weakly ordered where UC was not.* That moves us toward the reference behaviour
  rather than away from it — the host explicitly asks for `VIRTIO_GPU_MAP_CACHE_WC`, so
  Mesa's Venus path is written against WC semantics on native Linux, and its ring submission
  goes through a locked atomic that drains the WC buffers — but it is worth knowing if a
  blob ever gains a new consumer.
- *MTRRs cannot defeat this on the hardware we verify on.* The blob lives in a 64-bit
  prefetchable BAR above top-of-RAM, where firmware leaves `MTRRdefType = UC`, and
  (MTRR=UC, PAT=WC) is WC. Corroborated twice, because a recalled SDM table row is not
  evidence: Linux's `arch_phys_wc_add()` adds no MTRR when `pat_enabled()`, and
  `pat_x_mtrr_type()` consults MTRRs only for WB requests. The one configuration that could
  defeat it — an old-KVM Intel host with EPT `IPAT=1` — would equally defeat `18a7a9f`'s
  already-landed UC mapping, which demonstrably works.

**Diagnostics in-tree, all `false` at HEAD** — flip to `true`, measure, flip back before
committing. `c5abb8d` shipped with `DRM_STATS` on and `c27557f` had to undo it; the rule is
not decorative. All four line numbers below were re-verified against the tree at `73258ea`.

| Flag | File | Measures |
|---|---|---|
| `DRM_STATS` | `drivers/src/drm_device_interface.rs:1734` | flips, cursor up/mv, atomic, atest, cplane, `dmg_{full,rect,skip,px}`, `blobs`, `evpush`, `bo_{dumb,dumbret,blob,bhnd}` |
| `CURSOR_DEBUG` | `drivers/src/virtio_gpu.rs:342` | cursor queue setup + selftest |
| `mm::gap2::ON` | `mm/src/gap2.rs:17` | memfd/MAP_SHARED path + frame checksum sampler |
| `pci::RENDER_DEBUG` | `drivers/src/pci.rs:99` | per-frame DRM/FB/GPU/KMS/SND serial tracing |

**Reading the `[DRMSTAT]` BO census** (`73258ea`). The four `bo_*` fields are **derived**
from map `.len()` rather than kept by hand, so they cannot drift from the maps they
describe. `bo_bhnd` sustained above `bo_blob` — more gem handles than objects — is a
**handle leak**. `bo_blob` above `bo_bhnd` is the healthy converse: objects outliving their
handles because an exported dmabuf fd still pins them. `bo_dumbret` climbing monotonically
is the **retention leak**. `u64::MAX` in any field means the sample was skipped on lock
contention, not that the map was empty. **New `[DRMSTAT]` fields go at the END of the line,
never in the middle** — `c5abb8d` inserted five `dmg_*` fields mid-line and every
position-keyed parser downstream silently reported zero for everything after them.

Measured with the flag on: **zero `u64::MAX` across 69 + 67 + 76 samples**, no hang under
four `drmsmoke` cycles, a full `venustest`, and a 200 s live COSMIC session. `bo_dumb` went
0 → 1 → 0 per `drmsmoke` cycle without accumulating; `bo_dumbret` was 0 throughout; the blob
fields were 0 because this Mac cannot make blobs. **A non-reproduction, stated as such:** the
earlier 185 s census found live dumb buffers frozen at 4, and this COSMIC run read 0
throughout — because it was still in startup when capture ended (`flips_sub=1`, `atomic=0`,
the panel still connecting to the notifications daemon). It does not reproduce the 4 and it
does not contradict it. The bounded-not-climbing property rests on the `drmsmoke` cycles,
which did exercise the path.

**Instrument reliability — read this before trusting a number.** **Twenty-three** separate
instruments have now produced believable wrong numbers, or would have. They are grouped by
failure class rather than listed in order of discovery, because the class is what
generalises; the specifics are what make each one actionable.

*Class A — the instrument reported success it never measured.* Every one of these failed
**toward looking successful**, which is why they are the most dangerous class.

1. A serial `expect()` that searched backwards over an accumulated buffer and re-matched the
   *previous* command's end sentinel: every command after the first reported `rc=0` **without
   ever running**. Caught only because a log claiming `venustest` passed contained no
   `venustest` output. Take the buffer mark *before* sending, and number the sentinel per
   command.
2. `git apply --check … | head -10 && echo OK` reports success whenever `head` succeeds,
   because a pipeline's status is the *last* command's — so a patch that failed printed
   `APPLIES CLEAN` two lines below its own error text. Branch on the command itself
   (`if git apply --check …; then`). **Hit a second time, live, while reconciling this very
   file:** `pgrep -fl 'build-all.sh' | head -5 || echo "(no build running)"` printed
   *neither* a process list nor the fallback, because `head` exited 0 and swallowed `pgrep`'s
   "no match" status. The `||` branch is just as status-blind as the `&&` branch. Use
   `if pgrep -f …; then … else … fi`. Two independent instances now; this is the most
   frequently re-walked trap in the list.
3. `driver.py start` printed `QEMU started` over a guest that had already exited, because
   `aarch64_vars.fd` was missing. The serial log was 0 bytes, so every test read as *absent*
   rather than *failed* — in a suite that greps for PASS lines, indistinguishable from a run
   where nothing was asserted. Fixed in `cec0a04`; the lesson is that absence and failure
   must be distinguishable at the harness level.
4. **QEMU's Unix serial chardev serves one client at a time and discards whatever the guest
   emits while no client is attached.** `driver.py cmd` connects, reads for its timeout and
   disconnects, so under TCG x86_64 — slow enough for output to start after the reader has
   left — a whole command's output is lost and resurfaces mid-stream in the *next* capture.
   `drmsmoke`'s output appeared inside `scmrun.py`'s log, opening at `VERSION: PASS`, and
   `grep -c ': FAIL'` on the truncated capture returned **0**: indistinguishable from a clean
   run. Caught only because the positive control went silent on x86_64 after firing on
   aarch64 minutes earlier. Fix: hold one connection for the whole command, which
   `scripts/scmrun.py` does.
20. **A truncated build log is uninformative in BOTH directions, and it misled in the
    unexpected one.** A delegated lane's `tee`-redirected copy of `build-all.sh` desynced and
    stopped mid-run at `Output: mame-aarch64 (372M)` — 22 KB, **zero** `error`/`failed` lines.
    `grep -ciE '^error|error\[|failed'` returns **0**, *identical* to a clean build and the
    same shape as entry 4's `grep -c ': FAIL'` on a truncated capture. That is the expected
    trap. **What actually happened was the mirror image: the build had SUCCEEDED, and the
    truncated log plus an absent process led the coordinator to record it as killed.** The
    lesson is therefore stronger than "a truncated log can fake success" — a truncated log
    supports *whatever* you already suspect, and "no process running" is equally consistent
    with finished and with dead.
    **Cross-foot against the build's products, not its log.** One `ls` settled it:
    `f2fs-data0-aarch64.img` was written at **09:38:05**, two minutes *after* the log's last
    line at 09:36. Products have timestamps; a log only has content, and content is what
    truncation destroys. Corollary for delegation: require the success sentinel
    (`🎉 Build Complete!`) *or* a product mtime — never infer either outcome from silence.

*Class B — the instrument measured something, but not the thing.*

5. The Stage 0a registry dumper, caught before it lied. The natural build — extend
   `leandros-applet` — would have enumerated **cosmic-panel's embedded server**, because the
   panel hands each applet an inherited `WAYLAND_SOCKET` fd and `connect_to_env()` follows
   it. That server advertises `wl_compositor`, `wl_shm` and `xdg_wm_base` and no dmabuf:
   **indistinguishable from the true negative being hunted, and it satisfies the "the other
   globals prove the dump worked" sanity check**. The general lesson is that *a sanity check
   can be satisfied by the very failure it was meant to exclude*, so an instrument must
   establish **which** thing it measured, not merely that it measured something.
   `wl-globals` does: it ignores the environment, globs `wayland-*` in `$XDG_RUNTIME_DIR`,
   connects by explicit path, and the socket's identity is pinned by cosmic-session's own
   `got environmental variables from cosmic-comp: [("WAYLAND_DISPLAY", "wayland-1")]`.
6. **The one-client rule applies to QMP as well as to the serial chardev.** A
   damage-measurement run injected **zero** pointer moves because a second QMP client was
   opened while a resolution probe still held the first. An abort-on-zero-moves guard now
   makes that impossible to miss.
7. A blob write-bandwidth benchmark would have measured the wrong memory type. A plain
   `blob_id=0` HOST3D blob is answered `map_info=0x01` (CACHED) and maps **write-back**, so
   the `WRITECOMBINE` path is never touched and the result reads "inert" — which was one of
   the two outcomes the item explicitly predicted, so the false negative would have arrived
   in the expected shape and been believed. Assert the memory type you think you are
   measuring, in the log, on both arms.

*Class C — the extractor was wrong about the text.*

8. A parser keyed on field *position*: `m8_cursor.py`'s regex ran from `flip_us` onward, and
   `c5abb8d` inserted five `dmg_*` fields between `flip_us` and `curs_up`, so every field
   after the insertion point silently read **0** on a patched kernel. Parse `key=0xHEX`
   pairs order-independently (`m9_analyze.py` does).
9. A `^\S+: PASS$` extractor reported `PASS=0` because the serial console emits CRLF.
10. An all-caps summary filter scored `drmsmoke` **2/22** by eating real subtests whose names
    are upper-case, like `CREATE_DUMB: PASS`.
11. `waittest`'s trailing `WAITTEST: PASS` summary line, counted as a fifth subtest by a
    naive `grep -c ': PASS'`, is the whole reason its baseline was recorded as "5/0 or 3/2"
    against a source file with four cases.
    **It recurred on 2026-08-07 in a harness written that same day** (`m11_atomic_console.py`),
    which reported `waittest 5/0` on both arches. The committed log shows exactly four
    subtests — `wnohang_poll_until_exit`, `blocking_wait_for_exit`, `echild_no_children`,
    `wait_on_process_group`, all PASS — plus the summary line, and `grep -c ': PASS'` on it
    returns 5. The true result was a clean **4/0**. **Documenting a trap does not stop the next
    harness walking into it**, because each new harness re-implements the extractor from
    scratch. The durable fix is to stop counting lines and read the binary's own
    `failures = N` trailer, which every one of these test binaries prints and which cannot be
    inflated by a summary line. (`waittest` now holds **nine** cases, not four; the trailing
    summary line is unchanged, so the trap is unchanged and the counts a stale harness quotes
    will simply be wrong by one in a new place.)
12. **`grep` over a serial log sharing a pty with QEMU's trace stream.** With
    `-trace virtio_gpu_cmd_*` and no `-D`, every guest character triggers a console flush, so
    trace lines land *between* the guest's bytes: `present_addfb2: PASS` arrived as twenty
    single characters. `grep -a "present_"` found **2 of the 10** present subtests and
    reported nothing wrong — the eight missing ones looked exactly like eight subtests that
    never ran. The same shredding broke the harness's own sentinel, so an `rc=0` run was
    reported as a harness failure. Fix: `-D <file>`, so the trace stream never touches the
    pty.

*Class D — the test could not fail.*

13. `memfd_inflight_close` as first written could not fail, because the hazard window never
    opened. The same trap was walked into and *avoided* later: a `close(0)`-consequence check
    would have been vacuous, since `sys_fcntl` short-circuits `fd <= 2` and answers
    `F_GETFD` with a hardcoded `0` without consulting the fd table.

*Class E — the number was real; the window was wrong.*

14. A count delta that looked perfect: `virtio_gpu_cmd_ctx_submit` events per renderer
    lifetime came out 6 on HEAD and 7 patched — a clean +1. The same binary three times in
    one HEAD boot gave **6, 6, 7**. Venus notifies its ring opportunistically; the count
    floats and the +1 was noise.
15. A window-selection artifact: taking the *longest* zero-`evpush` window began before
    bring-up, averaged full-output repaints into the per-present figure, reported
    126,072 px and produced a **false failure of the idle positive control**. Take the
    *tail* of a quiet period — steady state by construction, rather than by choosing
    favourable samples.

*Class F — the environment moved under the measurement.*

16. A positive control that came back showing **only the prompt**, because the read window
    raced login settle. Re-running it passed — which is itself the argument for running a
    control rather than assuming one would have passed.
17. **Commands sent to a shell ~180 s after a COSMIC session launch do not execute** (or do
    not echo): the console is saturated by session output. Any session-probing design must
    **background its work early**, while the console is still responsive.
18. `driver.py cmd`'s shell-prompt heuristic swallowing error lines on TCG x86_64, where the
    guest is slow enough for the heuristic to break early.
23. **A single sample cannot tell "the pixels never arrive" from "the pixels had not arrived
    yet" — and it produced a confident, geometrically perfect FALSE FAILURE.** The aarch64 M4
    run 1 sampled each hold once, at +6 s, and **failed** phase B — having caught a solid
    rectangle of `vkwl`'s *previous* frame colour at **exactly** the right geometry: 151,868 px,
    478x318, fill 0.9991. Every structural check passed; only the colour was a frame stale, so
    the failure looked like a real one. The cause is a sentinel meaning less than it appears:
    **`HOLD READY seq=N` means the client returned from `vkQueuePresentKHR`, not that the
    compositor composited and flipped.** The client bursts its frames in seconds while the
    compositor is still draining. Fixed by sampling a *series* rather than a point, which
    converts the ambiguity into a number: in run 2 the window is **absent from the screen
    entirely** at +6 s and correct at +26 s. Run 1 is committed unedited, because a
    well-formed false failure is worth more in the record than a quiet re-run.
    **Open, ~1 line:** that harness's `HOLD_RE` can match a line only *partly* arrived over the
    8-byte-at-a-time serial link — it matched `secs=1` in run 2 phase C and zeroed that hold's
    sampling budget. It passed on sample #1 anyway, but it would otherwise have scored a hold
    it never sampled. Anchor the regex on a line end.
22. **`git add` skips ignored paths silently, so a commit can look complete while dropping
    exactly the evidence it exists to preserve.** `artifacts/.gitignore` excluded `*.png` and
    `*.log` to keep build output out and swept up every capture with it. `dc013c0` committed
    the aarch64 vkrender `results.md` and `precommit-pass-criteria.txt`; the three frames and
    both logs were dropped **without an error, without a warning, and with a clean
    `git status` afterwards** — the lane reported them as landed in good faith. Caught only
    because the next step was to re-census the PNGs and they were not there. Fixed by
    `!notes/**/*.png` / `!notes/**/*.log` (`03ee4ee`). **The general shape: an exclusion rule
    written for one category will silently capture anything that shares its file extension,
    and `git add <dir>` reports nothing.** After committing evidence, `git show --stat` and
    count the files. Every other entry in this list is about a measurement that lied; this one
    is about the measurement surviving at all.
21. **An in-guest polling loop wedged the guest shell outright.** Relaying a test binary's
    sentinels by redirecting its output to a file and polling with `$(grep … | tail …)` in a
    loop produced **20 minutes of total console silence** — no sentinel, no `M4: DONE`, not
    even the `cat` that followed the loop. Not root-caused; the working shape was to run the
    binary in the foreground and **cut the output at the source** (`vkwl` gained a `quiet`
    mode) rather than filter it downstream. Recorded as a landmine, not a diagnosis: if you
    need a long guest run to be quiet, make the *program* quiet.
19. A shared-scratchpad collision: a stale foreign results directory predating the run by
    ~40 minutes, with the wrong filenames, sitting exactly where a lane's output would be
    read from. Namespace scratch output per run, and stat the mtime before reading it.

Three rules follow, and all are cheap. **Run a positive control** — send a known-failing
command (`nosuchbinary_xyz42`) as the first command of every boot and confirm the harness
reports it failing; that single step catches 1, 3, 4, 16 and 18. **Prefer a structurally
distinctive observable over a count delta** — replacing a submit *count* with a
`(payload size, flag word, ring index)` histogram settled the syncobj question
unambiguously where the count had hidden the event entirely. **Cross-foot every number
against a second, independent source** — the test binary's own `failures = N` trailer is
what caught 9, and it is what pinned `venustest` at 108/0 this wave.

**Vulkan test-binary build findings, load-bearing for anyone rebuilding them.** `-std=c11`
does **not** compile against musl — strict ISO hides `clock_gettime`, `nanosleep` and
`CLOCK_MONOTONIC`; use `-std=gnu11`. Vulkan headers need `/usr/include/vk_video` as well as
`/usr/include/vulkan`, copied to a private dir — do not point `-I` at `/usr/include`, it
shadows the target libc's headers. The container recipe **cannot build aarch64 on the box**:
no docker, and podman pulls arm64 images but cannot execute them. Cross-compiling with
`zig cc` + `musl-dyn-link.sh` works, with two gotchas — `zig cc` enables UBSan by default
(link fails on `__ubsan_handle_*`, needs `-fno-sanitize=undefined`) and its driver silently
produces a **static** binary, which cannot `dlopen` the ICD. Corrected recipes:
`artifacts/notes/m9-m3-vulkan/build-vkrender-alpine-fixed.sh`,
`build-vkrender-aarch64-zig.sh`, and `m9-vkswap/build-vkswap-alpine.sh`. The Vulkan loader
stays unshipped: the ICD exports only `vk_icdGetInstanceProcAddr`,
`vk_icdNegotiateLoaderICDInterfaceVersion` and `vk_icdGetPhysicalDeviceProcAddr`, so it can
never stand in for `libvulkan.so.1`. `vkrender`'s `s2_checksum` is **printed but not
asserted** unless `VKRENDER_EXPECT_CHECKSUM=0x02C0FDC5` is exported; every comparison so far
has been done by hand.

**Evidence lives outside this repo.** Run logs, screenshots, research notes and test
harnesses are in `artifacts/notes/`. Design docs that are still
execution-ready are in `docs/design/`.

**Explicitly out of scope** (all degrade gracefully or are non-fatal): XWayland,
accountsservice, hotplug, multi-seat.

---

## COSMIC desktop: older open items

What is still open from the numbered items of 2026-08 (input, applets, busd, PAM, utmpx, VT
switching, Vulkan/M4, deferred work). The full history of each item, including the refuted
premises, is in `git log -p TODO.md` and `artifacts/notes/`.

**Desktop and sessions:**
- **D-Bus services still missing**: xdg-desktop-portal (no portal, screenshot or file chooser),
  timedate1, SwitcherooControl and the a11y bus. busd activation itself works, and lane
  `dbussvc` (`ports/sysbus`) added login1, locale1, UPower, power-profiles and PolicyKit1.
- **`cosmic-files`, `cosmic-edit` and `cosmic-store` are not built or staged.**
- **One permanent COSMIC patch remains**: `ports/cosmic-comp/0001-toplevel-info-weak-window.patch`
  (the closed-window texture leak, lane `weakwindow`). The goal is unmodified COSMIC, so it is a
  standing debt until upstream has an equivalent.
- **The minimize applet's un-hide is unverified.** An empty toplevel list and a wedged handler look
  the same in the item-9b capture; the test that separates them is opening a real toplevel and
  checking that minimize un-hides it.
- **Two COSMIC defects with observations but no root cause, not re-checked since 2026-08**:
  `cosmic-files-applet`'s `smithay-clipboard` thread panics on `Failed to create memory pool:
  OutOfMemory (12)` (the process survives), and `cosmic-notifications` intermittently logs
  `Failed to setup panel dbus server … Broken pipe` (also on control kernels).
- The greeter's cosmic-comp logs `eglMakeCurrent … BadDisplay` / `Failed to convert between
  dmabuf and EGLImage` on its way out after cosmic-greeter exits. Cosmetic: the scanout is
  released and the console comes back.

**PAM (item 12).** greetd, `su` and `sudo` authenticate through `ports/greetd/pam-leandros`
(Rust, with the environment table that greetd's session env depends on). The image still ships
`libpam.so.0`, the C shadow-auth shim built from `~/code/leandros-artifacts/m6-session-bins/src/libpam-shim`
(untracked, not a git repo), because `cosmic-greeter` is linked against it (`DT_NEEDED`), even
though the login role does not call PAM. **Open decision:** replace that shim with a Rust cdylib
(a new build shape: every in-tree `userland/` binary is static today) or port Linux-PAM (which
`dlopen`s its modules). Any replacement needs a guard falsified by mutation: a wrong password
must be shown to be rejected.

**utmpx (item 13), untouched.** Nothing in the tree implements it, and upstream greetd does not use
it. sshd keeps no utmp/lastlog either. Before implementing, establish a consumer: does anything we
ship read it (`who`, `w`, `last`)? Is the requirement the file format (`/var/run/utmp`,
`/var/log/wtmp`) or the libc interface (`setutxent`/`getutxent`/`pututxline`)? Without a consumer
the implementation is unfalsifiable.

**VT switching (item 14) residuals.** Six VTs, the VT/KD ioctls, the `VT_PROCESS` handshake, DRM
master scoped to the VT and the evdev VT gate all landed; what is left:
- A present already inside its handler when the switch happens can still land its pixels after the
  console's repaint: a visible flash, self-healing. Closing it means interlocking the present with
  the switch.
- **Asserted, not read:** that smithay maps `EACCES` to a recoverable `DrmError::Access` while
  `EPERM`/`ENODEV` tear the device down. The kernel also logs nothing when it refuses a present on a
  background VT (the `drivers` crate has no print facility), so there is no count.
- The evdev VT gate was exercised against `vttest`/`evsplit`, not against cosmic-comp.
- `EVIOCGRAB` is enforced, but exclusive *delivery* under a grab is not directly measured (needs
  host-injected events plus a second reader). `EVIOCREVOKE` is accepted and ignored.
  Ctrl+Alt+Esc is swallowed from all clients, so a COSMIC binding on it would never fire.

**GPU and Vulkan:**
- **A host-refused `RING_IDX` submit (item 1) is never completed by QEMU.** Since lane `ctrlq` the
  submit no longer spins, but the chain stays in flight and its descriptors are lost until the
  device answers. Real Mesa cannot reach it (its streams are valid Venus protocol), so it blocks
  nothing; whether to add a guest-side precondition, a distinct error or nothing is undecided. Do
  **not** refuse or rewrite `RING_IDX` kernel-side: Mesa sets it on every submit.
- **Cross-open dmabuf import (item 3), deferred, nothing scheduled.** `open_may_reach`
  (`drivers/src/drm_device_interface.rs`) scopes BOs to their owning DRM open. Worth doing for
  Venus importing a foreign dmabuf (Vulkan-to-Vulkan sharing between two guest processes; Mesa's
  importer refuses a BO whose `blob_mem` differs, so `RESOURCE_INFO` through an imported handle
  must report the original), a zero-copy client→compositor path, and `VK_KHR_display`. Design:
  `artifacts/notes/m9-crossopen-dmabuf/crossopen_design.md`.
- **virtgpu `FENCE_FD_IN` (sync-file import) and `in_syncobjs`/`out_syncobjs` in EXECBUFFER are
  not honoured** (logged once per shape): the driver layer cannot resolve the caller's fd table.
- **DRM properties cosmic-comp tolerates as unsupported**: `VRR_ENABLED`. Nothing optional is
  advertised on purpose; smithay degrades cleanly.
- **A permanent Vulkan-Wayland test would need a new build shape.** `vkwl.c` stays outside the repo
  as a spent probe. A Rust rewrite (~450-650 lines with `ash`) must `dlopen` the ICD, so it cannot
  be a static `scripts/build-userland.sh` binary; it needs a dynamic-musl mode (the
  `wl-globals`/`leandros-applet` recipe) and `wayland-client`'s `system` feature.

**Kernel and build, deferred:**
- **Still polled, no interrupt path**: virtio keyboard, blk, net and snd, and the aarch64 GPU
  control queue (x86_64 has MSI-X there; INTx via GIC SPI would be the aarch64 step, with the tick
  poller as the fallback everywhere).
- **Synthetic sysfs**: only `/sys/class/block` exists (lane `disks-rs`). The read-only
  `/sys/dev/char`, `/sys/class/drm`, `/sys/class/input` design in `docs/design/k4-drm-design.md`
  is deferred; the libudev shim's static table means no current consumer needs it.
- **A pre-existing buddy-slack leak on the ELF loader's eager→lazy split** (from the
  dynamic-linking wave), not re-measured since.
- **The image can silently ship without `libseat.so.1`/`libudev.so.1`**: with neither `zig` nor the
  `m4-input-ship` blob cache, `build-all.sh` skips the shim build, `mkfs` omits them and the build
  still succeeds; a COSMIC session then fails at runtime. Same shape as every
  `os.path.exists()`-guarded blob in `mkfs-f2fs-populated.py`.
- **Doom does not link relibc.** `../doomgeneric/Makefile.leandros` links
  `libleandros_libc.a`, whose allocator (`userland/libc/src/mem.rs`) is a ~20-line bump allocator
  over `brk(2)` with no free list and no `mmap` path. Start there when debugging a Doom allocation.
- **Crate layering:** `drivers` has no Cargo edge to the arch crates. Use a `#[no_mangle] extern
  "C"` symbol resolved at link time (as `arch_monotonic_ns` is for `servers/evdev`) before adding a
  dependency edge.
- **Build gotcha:** a userland test built with a bare `cargo build` instead of
  `scripts/build-userland.sh` lacks `-C relocation-model=static`, produces a PIE whose
  `.data.rel.ro` our loader never relocates, and faults at `__libc_start_main+0x44` with `CR2=0`
  before `main`.

---

## Housekeeping

- **x86_64/TCG COSMIC settle timing is stale — re-measure (clock fixed 2026-09-16).** The old
  ~700 s settle budget (wallpaper at ~240 s, full panel by ~600 s) was measured on the slow guest
  clock (`67c9ba1` fixed 5–36% drift across all accelerators); every wall-time figure recorded
  before 2026-09-16 is suspect. The underlying lesson stands regardless of the number: on the
  Mac x86_64 is the *slow* target (TCG, not KVM), and **a screenshot taken too early is
  indistinguishable from a desktop that never came up.**
- **Out-of-repo sibling that brush will not build without: `../crossterm`.** The cosmic-term
  output fix is a **fork of crossterm 0.29.0** at `~/code/crossterm` (box: `~/Projects/crossterm`,
  symlinked to `leandros-siblings/`, like every other sibling), wired in by
  `[patch.crates-io] crossterm = { path = "../crossterm" }` in **brush's** `Cargo.toml`.
  `cargo tree -i crossterm` must show **one** node with three parents (`brush-shell`,
  `brush-interactive`, `reedline`) — two divergent copies means the patch missed reedline's
  transitive dependency and the fix does nothing. The fork *is* a git repo (2 commits: an
  unmodified import, then the fix), so it is diffable against upstream; it is **not** a
  submodule of this repo, so nothing here fetches it. A missing `../crossterm` fails brush's
  build outright rather than silently, which is better than the shell-less-guest gotcha below.
- **brush's LeandrOS changes live in `ports/brush/patches/`** (0001-0005: the crossterm fork
  wiring, the `kill` builtin, cooperative yields, job control, stdio dup, child chdir) and are
  applied to the `../brush` sibling with `ports/brush/sync.sh apply ../brush`; `build_brush`
  checks the sibling is in sync.
- **Fresh-worktree gotcha: the guest boots with no shell.** `build-all.sh` and
  `mkfs-f2fs-populated.py` resolve the sibling repos as `$ROOT_DIR/../<repo>`, and an agent
  worktree's parent is `.claude/worktrees/`, not `~/code/`. The build **exits 0** and only
  prints `⚠️ brush source not found … skipping`; the failure appears at runtime as
  `login: exec failed` / `session ended, restarting login`, i.e. `/bin/login` execve()ing a
  shell that is not in the image. `brush`, `coreutils` and `bottom-leandros` symlinks were
  added under `.claude/worktrees/` alongside the pre-existing `doomgeneric`, `mame` and
  `relibc`, and are left there deliberately.
- **`/bin/wl-globals` is staged when the host binary exists** —
  `~/code/leandros-artifacts/m9-wlglobals/out/wl-globals-<arch>` — a HOST path, not the
  in-repo copy: `out/` holds built binaries and is gitignored. Same conditional pattern
  as `leandros-applet`. It is a measurement instrument (it enumerates the `wl_registry` of
  every `wayland-*` socket in `$XDG_RUNTIME_DIR` and exits); nothing in the session depends
  on it. **Both arches are now built and staged**; the crate's `.cargo/config.toml` already
  carried the x86_64 target section, it had simply never been exercised. Needs
  `cargo +nightly` — the default stable toolchain has no Linux musl targets installed.
  **Why, precisely, and it flips the day the crate moves in-tree:** this repo's
  `rust-toolchain.toml` already pins `nightly-2026-04-16` *and lists both musl targets*, so
  a crate **inside** the repo needs no `+nightly` at all. `wl-globals` and `leandros-applet`
  live in `~/code/leandros-artifacts/`, outside that file's scope, and neither carries a
  `rust-toolchain.toml` of its own — verified, not assumed — so they fall back to the
  default toolchain and the `+nightly` is genuinely required *there*. A toolchain pin is
  directory-scoped; do not carry a caveat across a directory boundary without re-checking it.
  **Correction worth keeping:** the `-C relocation-model=static` build gotcha (in *COSMIC desktop: older open items*) does **not**
  apply to this binary. `wl-globals`, like `leandros-applet`, is a genuine *dynamically
  linked* PIE with a real `PT_INTERP` (`/lib/ld-musl-<arch>.so.1`), built with
  `-C target-feature=-crt-static -C relocation-model=pic` against `m3-gl-stack/sysroot-<arch>`.
  The landmine is about Rust's self-relocating *static*-PIE, which is a different recipe.
  Applying it here would break a working build. Both arches verified to have identical ELF
  shape (DYN, 11 program headers, same order).
- **The artifacts tree is now versioned, in `artifacts/`.** It previously had **no version
  control at all** — `~/code/leandros-artifacts` is not a git repo, so `vkwl.c`, `vktest.c`,
  `ssp_guard.c` and every `build-*.sh` had no history and nothing to recover them from for the
  whole Vulkan arc. **Only authored material was imported**: sources, notes, patches and
  harnesses. The 41 GB / 131,505-file original holds vendored upstream trees (Mesa under
  `llvmpipe-lane/deps-*`, pipewire, musl) and build output, which are deliberately excluded
  and must be rebuilt from their recorded recipes; `artifacts/.gitignore` keeps them out.
  **The original directory still exists and was not deleted** — it remains the place where
  builds actually run, it is still hand-synced between the two machines, and it **diverged
  again during the pty/VT wave**. `artifacts/` being versioned does not make
  `~/code/leandros-artifacts` versioned; they are two trees with similar names, and the
  standing rule is to diff them before believing either about what is built.
  **`artifacts/` imports no `*.c` or `*.h`**, and `artifacts/.gitignore` blocks them so they
  cannot be reintroduced by a careless drop. The host-side C probes (`vkwl.c`, `vktest.c`,
  `ssp_guard.c`, `caps_probe.c`, `wlclient.c`) stay outside the repo and stay unversioned.
  That is deliberate — they are scaffolding that has already yielded its findings, and the
  standing rule is that a capability worth keeping gets **rewritten in Rust**, not imported
  as C.

  **State that rule accurately, because "the repo is Rust-only" is false and a grep refutes
  it in one command.** `git ls-files '*.c' '*.h'` returns **seven** files at HEAD: two
  vendored (`.limine-cache/…/limine.c`, `limine-bios-hdd.h`), one stray
  (`userland/libc/src/test_mmap.c`), and **four hand-written and actively maintained** —
  `ports/input-stack/shims/{libseat,libudev}/*.{c,h}`, with real commit history such as
  `0bed5ad`. The rule that actually holds, and that the shims satisfy rather than violate:
  **new capability code is Rust; C exists only where the artifact *is* a C ABI.** A shim
  whose entire job is to impersonate `libseat`'s symbol table has to be C — you cannot
  provide a C library's ABI without writing the C ABI. `vkwl` is not that: it is a client
  *of* an ABI, so nothing about it requires C. **Fixing the premise strengthens the rule.**
  Stated as an absolute, the first person to run that grep concludes the rule is dead.
- **Two spent instruments, kept but not pending.**
  `artifacts/notes/m9-damage-rootcause/damage_rect_dump.patch` (132 lines,
  one file, entirely inside the `DRM_STATS` gate) was applied on the box, produced the
  damage answer, and was reverted; it still applies clean and remains a usable clip-list
  dumper, but the question it was built for is closed, so it is not prepared work.
  `m9-dmabuf-lifetime/dmabuf_lifetime.patch` landed as `49399f9` — do **not** re-apply it;
  it is kept only because its companion `dmabuf_lifetime.md` is the reference for the
  refcount model.
- Run regression harnesses with `python3 -u` and **no pipe**: buffering makes a healthy
  background run look like a crash, and piping through `tail` gets the run reaped at exit
  144. Prefer `scripts/scmrun.py` (one process per command, one held serial connection,
  explicit pre-send drain, fixed read window, no `expect()`) over `driver.py cmd` for
  anything whose number will be quoted, and open every boot with a positive control.
- When host tracing is on, always pass `-D <file>`. A trace stream sharing the guest's pty
  interleaves per character and silently destroys both `grep` results and harness sentinels
  (instrument-reliability entry 12).
