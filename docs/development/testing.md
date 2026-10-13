# Testing

LeandrOS is tested by guest-side regression suites that run inside QEMU on both architectures, driven over the serial console. This page lists the suites, how to run them, and the rules that keep a passing result meaningful.

## Regression suites

The suites live in [`userland/`](../../userland) and are installed in `/bin` on the root image. Each prints a `PASS`/`FAIL`/`SKIP` line per case and exits non-zero on failure.

| Suite | Coverage |
|---|---|
| `memtest` | fork/CoW isolation, `mremap`, buddy allocator churn, `MAP_SHARED`, `fill_ram_no_leak` (allocate/verify/free at guest-RAM scale) |
| `uptrtest` | The kernel reading and writing user memory that is valid but not yet mapped — lazy anonymous mmaps, private file mmaps, the stack, copy-on-write pages after fork — and bad user pointers that must fail cleanly |
| `vfstest` | `rmdir`, cross-mount `rename` (including POSIX replace), `flock`/`fcntl` locking, permissions, chroot, xattrs, tmpfs files |
| `permtest` | Permission enforcement beyond `open(2)`: traversal, creation, removal, AF_UNIX `connect`, shared between tmpfs and F2FS |
| `f2fstest` | Direct, indirect and double-indirect block pointers, directories |
| `pthreadtest` | `pthread_create`/`join`, mutex, condvar, TSD, cleanup handlers |
| `timertest` | POSIX timers, `alarm`/`setitimer`, real `SIGALRM` delivery, timerfd `TFD_TIMER_ABSTIME`, sub-tick `clock_gettime`/timerfd intervals |
| `sigtest` | `sigaction` struct layout, delivery and return, `sigprocmask`/`sigpending`, `SIG_IGN`, `raise()`, timed futex waits under signals |
| `sigtest2` | Mask and disposition inheritance across `fork`/`pthread_create`, fault-handler `siginfo`, default-action kills, stop/continue/kill-while-stopped |
| `polltest` | `poll`/`select`/`epoll` real fd readiness, `epoll_wait` timeout and wake latency, pipe `POLLHUP` writer refcount across `dup` |
| `epolltest` / `wakepolltest` | epoll edge cases; same-thread `EPOLLET`/level eventfd re-arm; pipe and AF_UNIX blocking wakes; `FUTEX_WAIT_BITSET` absolute and past-deadline cases |
| `smpwaketest` | Cross-thread wake *latency* (a number, not a boolean): how long a blocked thread takes to return once a peer on another CPU wakes it |
| `scmtest` | `SCM_RIGHTS` fd passing, shared mmap, memfd seals and collisions, AF_UNIX socket nodes, tmpfs mounts, `mincore`, fork+exec fd inheritance |
| `forktest` / `waittest` / `sigchldtest` / `racetest` | Process lifecycle, `wait4`/`waitid`, `SIGCHLD`, SMP races and starvation, process-group wait semantics |
| `exectest` | `#!` scripts through `execve` with Linux `binfmt_script` semantics, nested interpreters, depth limit (`ELOOP`) |
| `jobtest` | Terminal job control on a PTY: `^Z` stops the foreground group, `SIGTTIN`/`SIGTTOU`, `tcsetpgrp` scoping, orphaned process groups |
| `ptytest` | `/dev/ptmx` + `/dev/pts/N`: the rings, termios line discipline, ioctl routing (also run as a non-root user) |
| `vttest` | The virtual consoles: VT/KD ioctls, switching, per-VT state |
| `killmt` | `kill -9` of a multithreaded process across several modes, including siblings spinning on other CPUs at the moment of the kill |
| `spawnwedge` | The musl thread/fork lock hand-off regression (`CLONE_PARENT_SETTID`): repeated `fork`/`posix_spawn` from a multithreaded process under allocator pressure. Run it at 200 spawns — the default 20 hid a second bug (an O(n) buddy `free`). |
| `idletest` | The system genuinely idles rather than busy-polling |
| `drmsmoke` | Raw-ioctl DRM smoke test against `/dev/dri/card0`: a real atomic commit, the console/scanout hand-off it must cause, page-flip timestamp classification, framebuffer and buffer-object lifetimes; `--hold` paints a checkable frame and leaves it on the scanout for capture |
| `evtest2` / `evsplit` | evdev virtio-tablet capabilities and poll; two concurrent readers of one evdev node each seeing the whole stream (Linux's per-open queues) |
| `venustest` | Venus/virtio-gpu 3D **transport** conformance: a non-empty host-populated capset, per-open GEM ownership, dmabuf/blob lifetime across `close`, PRIME import of a blob into another open (phase 5b), `SET_SCANOUT_BLOB` scanout of host memory (phase 7) |
| `dbusprobe` | busd `.service` activation over the raw D-Bus wire protocol, including the `ServiceUnknown` fast path |
| `privtest` | Privilege checks that must hold for an unprivileged process — credentials, `kill`, `mount`, capabilities, device nodes — with Linux's results. Run as root it also drops to uid 1000 for the unprivileged cases. |
| `pcachetest` | The unified page cache ([memory.md](../kernel/memory.md#the-unified-page-cache)): write then read, `pwrite` into cached pages, `MAP_SHARED` against `pwrite`/`pread` in both directions with `msync`, a forked child storing through its shared mapping, truncate shrink and grow with zeroed tails (also through a mapping), write past EOF, unlink while open, two processes alternating versions through separate descriptors, `O_APPEND`, `fsync`/`msync(MS_SYNC)` durability, and eviction (big files re-read under memory pressure must keep their checksums while the eviction counter moves). Prints FAIL reasons; the shared-mapping cases SKIP under `pcache=0`. Sub-commands `quick`, `mmapsync`, `mmaphold` and `verify` serve the crash test below |
| `nettest` | IPv4 basics through the net server with Linux's results: ICMP raw and ping sockets, non-blocking and `EINTR` cases, `MSG_PEEK`, UDP, TCP half-close and socket options, DNS over UDP and TCP |
| `syscalltest` | `splice`/`tee`/`vmsplice` (short counts, offsets, blocking and `SPLICE_F_NONBLOCK`, the uutils `cat` broker-pipe shape on tmpfs and on f2fs), `rseq` (registration rules, `cpu_id`, abort on a signal and on preemption, `SIGSEGV` on a bad signature, fork/thread/exec lifetime) and the pidfd family (poll/epoll readiness at exit, `waitid(P_PIDFD)`, `pidfd_send_signal`, `CLONE_PIDFD`, `pidfd_getfd`, of an inet socket and listener too), inet sockets shared across fork (`inet_fork_shared`) and `fstat`/`readlink` of sockets and epoll fds (`fstat_sock_epoll`), with Linux's results |

Not suites but test tools the lanes added:

- **`pagecache-crashtest.py`** (in `.claude/skills/run-leandros/`) is the fsync-ordering check for the page cache. `pcachetest mmapsync`/`mmaphold` store into a file through `MAP_SHARED` and keep it mapped, so the data exists only as a dirty page-cache page; the script then syncs it (`fsync`, `fdatasync`, `syncfs`, `sync` from the mapping process; `sync -d` and `sync(2)` from outside), SIGKILLs QEMU and checks that every byte came back after the reboot (`pcachetest verify`); it also covers `write` + `sync -d` and `msync(MS_SYNC)`. It exists because per-file fsync skips the checkpoint for a file it thinks is clean, so dirty pages must reach f2fs as writes before that decision: with that flush disabled all six cases lose their data, with it all survive.
- **`fsbench`** ([`userland/fsbench`](../../userland/fsbench/src/main.rs)) is the filesystem latency bench. `read`, `walk`, `mmapdir` and `tick` came with lane fsio; `statlat <path> <seconds> [period_ms]` stats a path every period and prints p50/p99/max (the UI thread's view of a filesystem convoy); `dbwrite <file> <rate> [seconds] [file_kib]` is a database-like writer (random 4 KiB `pwrite` + `fsync`) that prints the fsync latencies; `statfs <n> [path]` times `statfs`; `statwalk <dir> <seconds>` does `getdents` + `stat` of every entry and prints per-call latency; `stallprobe <colddir> <file> <seconds> [max_ms]` runs a database-like writer, a 2 MiB + fsync bulk writer and a cold `statwalk` while the parent stats `/` (always cached) every 20 ms, and exits 1 if that stat ever waits longer than `max_ms`. Every output line starts with `FSBENCH`. Run it on the emulated slow stick (`usbms.wdelay_us=500000` or the merge model, [filesystems.md](../servers/filesystems.md#kernel-command-line-knobs)); on a fast virtio disk none of the stalls it hunts exist.
- **`killmt`** has `fs_io` and `fs_exit` modes (a thread exiting the group while a sibling is parked in filesystem I/O), the reproducer for lane loginhang.

`vktest` (an external binary built from the Mesa port) is the end-to-end Vulkan check: it `dlopen`s the Venus ICD, enumerates physical devices and creates a logical device on the real host GPU.

Programs link against **relibc** for full POSIX compatibility, against the lighter `leandros-libc` shim for `no_std` Rust programs, or against **musl** for ported software. Suites that need TLS bring-up (`relibc_start_v1`) for real `pthread`/`errno`/`sigaction`/`epoll` support link `librelibc.a` directly. `spawnwedge` and `killmt` are std/musl crates with their own `build.sh`, because they must exercise musl's own `pthread_create`/`fork` protocol.

`privtest`, `nettest` and `syscalltest` are deliberately C: the same source builds for a Linux host (`cc privtest.c`) and for LeandrOS (`zig cc -target <arch>-linux-musl -static`), so every expectation is checked against a real Linux kernel first. They are the only C tests in the tree; new tests are Rust ([conventions](conventions.md#rust-only)).

Some suites need particular conditions:

- **DRM master.** `drmsmoke`'s KMS cases (`ATOMIC_COMMIT`, `SETCRTC`, page flip, …) need exclusive DRM master. When the greeter's compositor holds it — the default boot — they **SKIP** with a one-line diagnosis rather than fail. To exercise them, stop the compositor first: `touch /etc/leandros/text-login; kill $(cat /run/greetd-init.pid)` (and remove the marker afterwards). Do not start Mesa clients during a `drmsmoke` run: its v3d block re-arms the device identity for every client while it runs (it takes a lock so two `drmsmoke` instances serialise).
- **A blob-capable GPU.** `venustest` needs a `--venus` host; on a host without one (such as a Mac) most phases report FAIL, not SKIP, by design.
- **Root.** `vfstest`'s permission and chroot cases, `privtest`'s positive cases and most of the suites are meant to run as root; run as `leandro` they fail with `EPERM` by design, and a failed non-root run can leave residue on the persistent image that makes a later root run fail too.
- **Network.** `nettest` reads its gateway from `/proc/net/route` and its DNS server from `/etc/resolv.conf`, so it runs unchanged on SLIRP and on `socket_vmnet`; its DNS cases resolve `example.com`, so the host needs internet access.

## Running the suites

The standard runner is `runtests.py` ([workflow](workflow.md#tools-built-on-the-driver)):

```sh
python3 .claude/skills/run-leandros/runtests.py aarch64 mytag                 # regress suite
python3 .claude/skills/run-leandros/runtests.py x86_64 mytag --repeat 3       # three fresh boots
python3 .claude/skills/run-leandros/runtests.py aarch64 drm --suite drm --virgl
python3 .claude/skills/run-leandros/runtests.py x86_64 one /bin/ptytest       # any commands
```

It boots, logs in as root on the serial console and runs each command through the driver's sentinel-framed `run`, so interleaved kernel lines cannot turn a finished run into an unknown status. The named lists are `regress` (`sigtest`, `sigtest2`, `memtest`, `scmtest`, `polltest`, `forktest`, `exectest`, `pthreadtest`, `epolltest`, `timertest`, `jobtest`, `waittest`, `sigchldtest`, `vfstest`, `nettest`, `pcachetest`, `syscalltest`), `net` and `drm`. Output goes to `$FFSESSION_OUT/tests-<tag>/` (default `/tmp/ffsession`): every command's output, the serial log, QEMU's full serial log and `summary.json`; the exit status is 0 only if every command returned 0 with a known status.

For a single long command by hand, [`scripts/scmrun.py`](../../scripts/scmrun.py) keeps one serial connection open and reads until an optional completion marker. A persistent reader is necessary: QEMU drops serial output while nothing is attached, and a long test run then looks like a hang. Build the marker so that it does not appear in the command line itself — the tty echoes the command, and a marker matched from the echo ends the read before the test has printed anything (`echo "M13""RC=$?"` is typed as `M13""RC=` and printed as `M13RC=`).

Integration verification of a wave runs the full battery on both architectures — every suite above, a graphical session on each GPU path, VT switching, power-off and restart, `privtest` as root and as `leandro` — and checks the serial logs for `panic`, `[WDOG]`, `[PF]`/SEGV (other than the suites' intentional faults) and `[BUDDY]` lines ([workflow](workflow.md#lanes-and-waves)).

## Fresh images

Run the suites against a **fresh disk image** when the result matters. Images persist across boots, and a suite that has already run against one can leave state that changes the next run's outcome — `O_TRUNC` clears a file's data but not its xattrs, which is exactly the sort of thing that manufactures a phantom architecture-specific failure. `vfstest` in particular leaves residue (`/tmp/jail`, xattr test files) and should run once per fresh image.

`f2fs-data1-<arch>.img` (the persistent `/data`) is a copy of data0, so regenerating only data0 leaves it dirty; regenerate both. A rebuild (`./scripts/build-all.sh --arch <arch>`) writes fresh copies of both; for parallel runs, clone them copy-on-write rather than sharing one pair ([workflow](workflow.md#build-outputs-and-image-provenance)).

## Show a new guard failing before you trust it passing

Every recent guard in this tree was run against the unfixed kernel first, against a prediction written down in advance, and several were falsified afterwards by mutating the fix back out and checking that the *predicted* subtests failed with the *predicted* messages. That matters because the most common way to ship a useless test here is for it to pass by not running: a passing test bounds only what it executed, and `scmtest`'s `fork_exec_inherit` passed for months while never once calling the `shutdown(2)` that was broken.

Related practices:

- **Assert that the assertions ran.** `venustest`'s blob phase carries a gate asserting its own assertions executed, which is what distinguishes "passed" from "silently skipped". "The ioctl returned 0" proves nothing when the risk is a silently wrong wire protocol.
- **Assert on your own objects.** A `drmsmoke` case that compared a device-wide object count flaked one run in four, because the greeter's compositor allocates in the same registry; it now asks the kernel about the specific object it created (refcount 2 with handle and framebuffer, 1 after destroy, 0 after close).
- **A test can race too.** `waittest`'s `wait_on_process_group` "kernel flake" was a race in the test (the child's `setpgid(0,0)` against the parent's `waitpid(-pid)`), deterministic on one host. Before blaming a change, run the same test against the unmodified kernel (one way: `mcopy` a baseline `kernel.elf` into a copy of the boot image).
- **Suspect the harness when output looks like escape codes.** A coreutils score of 51/105 was the harness: it returned before the command ran and dropped the one output line glued to a bracketed-paste sequence. The true score was 104/105 ([methodology](methodology.md)).

## See also

- [Workflow](workflow.md) — the driver and runners.
- [Debugging](debugging.md) — when a suite stalls or goes silent.
- [Methodology](methodology.md) — controls, refuted premises.
- [Programs](../userland/programs.md) — the non-test userland.
