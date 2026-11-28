# Testing

LeandrOS is tested by guest-side regression suites that run inside QEMU on both architectures, driven over the serial console. This page lists the suites, how to run them, and the rules that keep a passing result meaningful.

## Regression suites

The suites live in [`userland/`](../../userland) and are installed in `/bin` on the root image. Each prints a `PASS`/`FAIL`/`SKIP` line per case and exits non-zero on failure.

| Suite | Coverage |
|---|---|
| `memtest` | fork/CoW isolation, `mremap`, buddy allocator churn, `MAP_SHARED`, `fill_ram_no_leak` (allocate/verify/free at guest-RAM scale) |
| `uptrtest` | The kernel reading and writing user memory that is valid but not yet mapped — lazy anonymous mmaps, private file mmaps, the stack, copy-on-write pages after fork — and bad user pointers that must fail cleanly |
| `vfstest` | `rmdir`, cross-mount `rename` (including POSIX replace), `flock`/`fcntl` locking, permissions, chroot, xattrs, tmpfs files |
| `chroottest` | `openat2` (`open_how` versioning, `RESOLVE_*`), `vfork` and `clone(CLONE_VFORK)`, `clone3` (versioning, pidfd, stacks), `CLONE_PIDFD` `EMFILE`, paths longer than 255 bytes (also inside a chroot whose root is itself that long), `/proc` and `/dev` mounted into a chroot, bind mounts; checked against a Linux host |
| `permtest` | Permission enforcement beyond `open(2)`: traversal, creation, removal, AF_UNIX `connect`, shared between tmpfs and F2FS |
| `f2fstest` | Direct, indirect and double-indirect block pointers, directories |
| `pthreadtest` | `pthread_create`/`join`, mutex, condvar, TSD, cleanup handlers |
| `timertest` | POSIX timers, `alarm`/`setitimer`, real `SIGALRM` delivery, timerfd `TFD_TIMER_ABSTIME`, sub-tick `clock_gettime`/timerfd intervals |
| `sigtest` | `sigaction` struct layout, delivery and return, `sigprocmask`/`sigpending`, `SIG_IGN`, `raise()`, timed futex waits under signals, the lock-free signal hint (`sigtest sigfast [N]`: `RUN_QUEUE` holds per syscall from `/proc/rqhold`, cross-CPU posts that must not be lost) |
| `sigtest2` | Mask and disposition inheritance across `fork`/`pthread_create`, fault-handler `siginfo`, default-action kills, stop/continue/kill-while-stopped |
| `polltest` | `poll`/`select`/`epoll` real fd readiness, `epoll_wait` timeout and wake latency, pipe `POLLHUP` writer refcount across `dup` |
| `epolltest` / `wakepolltest` | epoll edge cases; TCP `EPOLLET` not re-firing on an idle writable socket; wake isolation (unrelated AF_UNIX traffic must not wake a waiter on other sockets, timerfds or a foreign epoll); the ready list (level stays reported, edge once, bursts, ring overflow, nested, `DEL` while parked, `ONESHOT` re-arm) — run it with and without `epoll.ready=0`; `epolltest bench` prints kernel ns per call. `wakepolltest`: same-thread `EPOLLET`/level eventfd re-arm; pipe and AF_UNIX blocking wakes; `FUTEX_WAIT_BITSET` absolute and past-deadline cases |
| `smpwaketest` | Cross-thread wake *latency* (a number, not a boolean): how long a blocked thread takes to return once a peer on another CPU wakes it |
| `scmtest` | `SCM_RIGHTS` fd passing, shared mmap, memfd seals and collisions, AF_UNIX socket nodes, tmpfs mounts, `mincore`, fork+exec fd inheritance |
| `forktest` / `waittest` / `sigchldtest` / `racetest` | Process lifecycle, `wait4`/`waitid`, `SIGCHLD`, SMP races and starvation, process-group wait semantics |
| `shmtest` | Shared anonymous memory across `fork()` ([memory.md](../kernel/memory.md#shared-anonymous-memory)): pages first touched after the fork, in both directions, across several forks, `munmap`, `MADV_DONTNEED`, `mprotect`, threads, `/dev/zero`, memfd, `mremap`, and release on the last reference |
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
| `udptest` | UDP semantics and readiness edges, checked 22/22 against a Linux host first: `EPOLLET` edges of UDP, ICMP and TCP listeners, connected-UDP filtering and `connect(AF_UNSPEC)`, `MSG_TRUNC`, `getsockname`, `EADDRINUSE` for UDP and TCP, `shutdown`, socket options, UDP GSO (`UDP_SEGMENT`), `sendmmsg`/`recvmmsg`, a wildcard server connecting to a peer, bursts, large and zero-length datagrams |
| `pstest` | `/proc/<pid>/comm` and `prctl` names (truncation, permissions, exited processes), `/proc/<pid>/task/<tid>/` with raw-clone threads (listing, per-thread `stat`/`status`/CPU, writes through task paths), argv-backed `cmdline` and `environ` including in-place retitles, `tty_nr`/`tpgid` of a pty session, and `ps` (default selection, `aux`, `-ef`, `-o`, `-p`, `-T`/`-L`, `-u`/`-U`/`-C`/`-t`) — 74 checks |
| `sudotest` | su and sudo end to end as root, `leandro` (wheel) and `cosmic-greeter` (not wheel): ids and groups, environments, passwords, the credential cache, policy denial ([security](../servers/security.md#su-and-sudo)) |
| `nettest` | IPv4 basics through the net server with Linux's results: ICMP raw and ping sockets, non-blocking and `EINTR` cases, `MSG_PEEK`, UDP, TCP half-close and socket options, DNS over UDP and TCP |
| `syscalltest` | `splice`/`tee`/`vmsplice` (short counts, offsets, blocking and `SPLICE_F_NONBLOCK`, the uutils `cat` broker-pipe shape on tmpfs and on f2fs), `rseq` (registration rules, `cpu_id`, abort on a signal and on preemption, `SIGSEGV` on a bad signature, fork/thread/exec lifetime) and the pidfd family (poll/epoll readiness at exit, `waitid(P_PIDFD)`, `pidfd_send_signal`, `CLONE_PIDFD`, `pidfd_getfd`, of an inet socket and listener too), inet sockets shared across fork (`inet_fork_shared`) and `fstat`/`readlink` of sockets and epoll fds (`fstat_sock_epoll`), with Linux's results |

Not suites but test tools the lanes added:

- **`pagecache-crashtest.py`** (in `.claude/skills/run-leandros/`) is the fsync-ordering check for the page cache. `pcachetest mmapsync`/`mmaphold` store into a file through `MAP_SHARED` and keep it mapped, so the data exists only as a dirty page-cache page; the script then syncs it (`fsync`, `fdatasync`, `syncfs`, `sync` from the mapping process; `sync -d` and `sync(2)` from outside), SIGKILLs QEMU and checks that every byte came back after the reboot (`pcachetest verify`); it also covers `write` + `sync -d` and `msync(MS_SYNC)`. It exists because per-file fsync skips the checkpoint for a file it thinks is clean, so dirty pages must reach f2fs as writes before that decision: with that flush disabled all six cases lose their data, with it all survive.
- **`fsbench`** ([`userland/fsbench`](../../userland/fsbench/src/main.rs)) is the filesystem latency bench. `read`, `walk`, `mmapdir` and `tick` came with lane fsio; `statlat <path> <seconds> [period_ms]` stats a path every period and prints p50/p99/max (the UI thread's view of a filesystem convoy); `dbwrite <file> <rate> [seconds] [file_kib]` is a database-like writer (random 4 KiB `pwrite` + `fsync`) that prints the fsync latencies; `statfs <n> [path]` times `statfs`; `statwalk <dir> <seconds>` does `getdents` + `stat` of every entry and prints per-call latency; `stallprobe <colddir> <file> <seconds> [max_ms]` runs a database-like writer, a 2 MiB + fsync bulk writer and a cold `statwalk` while the parent stats `/` (always cached) every 20 ms, and exits 1 if that stat ever waits longer than `max_ms`. Every output line starts with `FSBENCH`. Run it on the emulated slow stick (`usbms.wdelay_us=500000` or the merge model, [filesystems.md](../servers/filesystems.md#kernel-command-line-knobs)); on a fast virtio disk none of the stalls it hunts exist.
- **`trialwatchtest.py <arch>`** checks the trial watch end to end through `remote.py deploy-kernel` on a VM: a 20 s `set_link net0 off` blip must not reboot the trial, a lasting cut must reboot it after `netwatch.reach=40` and the next boot must be the `good` slot, and a promoted trial with `netwatch.trial_max=1` must print `disarmed (promoted)` and stay up.
- **`netwatchtest.py <arch>`** checks the `netwatch=` trial-kernel watchdog ([remote.md](remote.md#the-network-watchdog-netwatch)): a boot with no NIC (`LEANDROS_NIC=none` in the driver) must print the `[netwatch]` line and reset, and a boot with a NIC must stay up past the deadline.
- **`wifihtbench.py`** sends N MB host → guest over the simulated `wifi0` and prints the `ht:`/`rx_ba:` lines of `/proc/net/nic` — the reorder-buffer bench for [802.11n](../hardware/drivers.md#wi-fi-qos-ht-and-block-ack).
- **`netshape=`** (a kernel knob, [networking.md](../servers/networking.md#netshape-link-emulator)) shapes a QEMU NIC like a Wi-Fi link (rate, queues, delay, loss) for TCP-recovery tests.
- **`killmt`** has `fs_io` and `fs_exit` modes (a thread exiting the group while a sibling is parked in filesystem I/O), the reproducer for lane loginhang.

`vktest` (an external binary built from the Mesa port) is the end-to-end Vulkan check: it `dlopen`s the Venus ICD, enumerates physical devices and creates a logical device on the real host GPU.

Programs link against **static musl** (the `MUSL_LINKED` list in `scripts/build-userland.sh`) for full POSIX behaviour (real `pthread`/`errno`/`sigaction`/`epoll`), against the lighter `leandros-libc` shim for `no_std` Rust programs, or against **musl** for ported software. `spawnwedge` and `killmt` are std/musl crates with their own `build.sh`, because they must exercise musl's own `pthread_create`/`fork` protocol.

`privtest`, `nettest`, `udptest` and `syscalltest` are deliberately C: the same source builds for a Linux host (`cc privtest.c`) and for LeandrOS (`scripts/toolchain/<arch>-linux-musl-cc -static`), so every expectation is checked against a real Linux kernel first. They are the only C tests in the tree; new tests are Rust ([conventions](conventions.md#rust-only)).

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

It boots, logs in as root on the serial console and runs each command through the driver's sentinel-framed `run`, so interleaved kernel lines cannot turn a finished run into an unknown status. The named lists (`--suite`, repeatable) are:

| Suite | Commands |
|---|---|
| `regress` (default) | `sigtest`, `sigtest2`, `memtest`, `scmtest`, `polltest`, `fdmaxtest`, `pipebw`, `forktest`, `shmtest`, `exectest`, `pthreadtest`, `epolltest`, `timertest`, `jobtest`, `waittest`, `sigchldtest`, `vfstest`, `nettest`, `udptest`, `pcachetest`, `procmax`, `syscalltest`, `chroottest`, `netctltest`, `pstest`, `sudotest`, `nmtest --quick` (mock), host `sshsmoke.py`, and last host `wifisimtest.py` (it reboots into its own VM) |
| `net-e2e` | `netctltest`, `nmtest` (mock, full and `--ro`), host `nmrealtest.py` and host `netroamtest.py` (each in its own boot) |
| `net`, `udp`, `procps`, `netctl` | `nettest`, `udptest`, `pstest`, `netctltest` alone |
| `wifisim`, `netroam` | `wifisimtest.py`, `netroamtest.py` alone |
| `ssh`, `sshfull` | host `sshsmoke.py`, `sshsmoke.py --full` (keys, sftp/scp, git over ssh/https) |
| `drm` | `drmsmoke` (needs `--gpu virgl` or `--venus`) |

Suites added since 2026-10-18 (all in `SUITES` of `runtests.py`, or commands in `regress`). Host-side `*.py` scripts are `host:` commands that boot their own VM with a specific disk, cmdline or fault injection:

| Suite or command | What it guards |
|---|---|
| `uaccesstest` (regress) | Destinations that are valid but untouched (console, pipe, `readv`, file, socket, pty, anonymous mmap) are faulted in rather than `EFAULT`; bad ones fail cleanly |
| `epolltest cost` / `abi` / `stat` / `churn [secs] [tfork]`, `polltest` | Per-call epoll cost, ABI details, the stat table, and the churn stress (waiters, a thread spawner, a pipe poker, children exec'ing from a non-leader thread; `tfork` forks from the spawner thread). Suites `epollcost`, `epollabi` |
| `pthreadtest churn 5000` (regress) | Thread create/join/detach leaves the VMA count, `VmSize` and RSS flat (fails on >16 VMAs, >16 MB, >2 MB growth) |
| `shmtest memfd_lazy`, `shmtest bench` | Lazy `MAP_SHARED` of memfd/tmpfs: two mappings plus `pwrite`/`pread`, a kernel copy into an untouched page, `close(fd)` then touch, a forked child write; a bench of untouched and touched cost |
| `mmapbench [iters]` | `mmap`/`touch`/`munmap` microbench (anonymous and memfd, with busy siblings) used for the shootdown work |
| `killmt <n> proclife` (regress) | Exec de-thread against `SIGKILL`, a fatal signal, a sibling fault and a concurrent `clone` (`exec_kill`, `exec_exit`, `exec_sibterm`, `exec_sibfault`, `exec_clone`) |
| `pstest` (`test_kthread_exit`) | pid 1 stays init, returning kernel threads leave no process behind |
| `schedstress short`, `futextest` | Scheduler stress; futex lost-wake guard |
| `kexec -l … && kexec -u` (regress); `kexectest`; suites `kexecfdt`, `kexecblk` | Load/unload smoke; chained kexec generations (`kexecfdt` leaves the VM kexec'd gen0..gen4 and checks the FDT block is returned); kexec with the root on a block module |
| `f2fscptest`, `f2fscrashtest` (`--linux`: knob `f2fs.lxchain=1`, crashed image recovered by Linux on the box + `fsck.f2fs`; suite `f2fscrashlx`, opt-in), `f2fsfulltest`, `f2fsorphantest`, `f2fsgctest` (`.py`) | f2fs checkpoint, power-cut, near-full, orphan and GC behaviour; `F2FS_TEST_LEGACY=1` selects the old-format fixture where one remains |
| `blkslow`, `blkmany`, `blkfault`, `nvmefault`, `nvmeebs`, `rootpick`, `stdev`, `pcibridge` | Block layer: slow media, many devices, injected faults, NVMe reset after a timeout, Amazon EBS quirks (`nvmeebs`: by-id links, auto-writable root disk, 300 s stall cap), the root not depending on virtio probe order, two disks cloned from one image (`stdev`: distinct device identity), PCI-PCI bridge chains on AArch64 |
| `nitrox86` | x86_64 on an EC2 Nitro-like platform: no PIT, HPET, i8042 or display device (serial only); `/bin/timertest` plus a check that the TSC frequency came from CPUID/kvmclock and the PIT was seen absent |
| `usbisoch`, `usbstalltest`, `usbunplugtest`, `udevhottest` | xHCI isochronous pipes, stalls, mass-storage unplug/re-plug, hot-plugged HID reaching the compositor |
| `ec2init` | `ec2inittest.py`: `/bin/ec2init` against `scripts/fake-imds.py` (IMDSv2 token semantics enforced) behind a slirp `guestfwd` (`LEANDROS_GUESTFWD`); key lands in root's and leandro's `authorized_keys` (ssh login with it), hostname, once per instance id |
| `nettxtest`, `netidle`, `linkmon`, `netmetric`, `udppeer`, `inload` | Net daemon sleep/wake behaviour on a full TX queue and on idle, link-change interrupts, metrics, UDP peers, a heavy-traffic input-load guard |
| `listentag`, `nettag2`, `vfstag`, `kparks`, `pollnarrow` | Narrow parking: a TCP listener's pollers, blocked VFS waiters, console poll/epoll and `VT_WAITACTIVE` park on their own key and wake on their event |
| `trialwatchtest.py`, `psklogtest.py`, `straysweep`, `serialkbd`, `serialxfer`, `powerctltest.py` | The netwatch trial watch; no PSK in logs or `/proc/cmdline`; init's stray sweep not killing sshd threads; serial-console bytes never appearing as keyboard events; serial file push; power off from the greeter and a user session |
| `rpi5check` | The rpi5 kernel compile-checks inside `runtests` |
| `permtest` (not in regress) | Includes the backlight group cases; run it explicitly |
| `bttest`, `kmodctltest`, `netlinktest`, `proctest`, `strtest`, `floattest` | Bluetooth host core, module control, netlink, `/proc`, relibc string conformance, hard-float C ABI |

Added in the 2026-11-11 to 11-17 wave:

| Suite or command | What it guards |
|---|---|
| `slow` (group) | The long host tests kept out of the ten-minute `regress`: `f2fsorphantest`, `f2fscptest`, `f2fscrashtest`, `pagecache-crashtest`, `usbunplugtest`, `powerctltest`, `f2fsfulltest` (and `trialwatchtest`, which the full `regress` also runs, scheduled first). Each has its own timeout (`HOST_TIMEOUTS` in `runtests.py`) because a single 300 s default either killed them or hid a hang. Gate with `--suite slow` |
| `virgl` (group) and `audio-session`, `video`, `clickmove`, `udevhot`, `coreutils` | `host:virglsessiontest.py <name>`: each boots a virgl COSMIC session of its own and checks one thing (audio reaching the sink, video playback, a click moving the pointer, a hot-plugged device, a brush guest running coreutils). `virgl` runs the first four |
| `portalshot` | A live session where the portal `Screenshot` call must return response 0 and a non-empty PNG. The probe grants the PermissionStore permission first, as the Access dialog would, otherwise the portal waits on a dialog nobody answers |
| `vkwl` | x86-64 `--venus`: a Vulkan swapchain on a Wayland surface into cosmic-comp, frame callbacks counted |
| `longline` and `rawline` | The serial input path: an 85-step command of about 4 KB must run, and (`rawline`) typed as one unpaced burst it must arrive above a minimum end-to-end rate. They exist because of the COM1 receive-FIFO trigger (set to 14 bytes on x86-64 so console input is no longer starved) |
| `execdev` (regress) | an `ET_EXEC` image whose segments sit at the QEMU virt GIC and UART addresses must run: every aarch64 user page-table root used to carry kernel-only identity pages there, which shadowed the user mapping and made MAME (linked at lld's default base) fault forever. Fixed in `arch/aarch64/src/paging.rs` |
| `harness` | `harnesstest.py`: the harness itself (line-buffered output, automatic `-D` when host tracing is on, quoted commands surviving the ssh hop) |
| `podsmoke`, `podbridge`, `podpasta`, `podpause` | Root and rootless `podman run`, the default bridge, pasta port forwarding, pause/unpause |

Added in the 2026-11-17 to 11-28 wave (suite names as in `SUITES`; `host:` scripts boot their own VM):

| Suite or command | What it guards |
|---|---|
| `flatpak`, `appstream` | `flatpaktest.py`: a network-free Flatpak run against a local OSTree repo and a `--user` remote of its own (install, run under the real `bubblewrap`, on both architectures); `appstreamtest.py`: the appstream refresh against a local `file://` OSTree `appstream2` branch, using the guest's architecture |
| `kmodbuild`, `kmodsuite` | `kmodbuildtest.py` builds the modules; `kmodsuitetest.py` boots a private `=m` kernel (build-id checked) and runs the real `kmodctl` to load and unload `virtio_net`, `nvme`, `iwlwifi`, `brcmf` and `sdhci`. Both are in the default list |
| `v6inject` | `v6injecttest.py`: router advertisements (including the MTU option), DAD claims and a DHCPv6 peer injected from a Linux peer on a second NIC (ten cases) |
| `leakdns` | Sixteen leaked uid-1001 user-namespace holders must not break DNS and the stack teardown that follows |
| `conloss` | `conlosstest.py` and `/bin/conloss`: console lines survive a flood, and `/proc/sysstat` carries the UART drop counters; it compares only its own run's lines, since a shared VM log holds earlier ones |
| `failevid` | A failing suite saves its serial log, its full output and the host load (and, for a timeout, a CPU sample of the QEMU process) so the failure carries its own evidence |
| `deadtimer` | `timertest deadtick` re-arms a stall inside one boot until the x86-64 LAPIC periodic tick really dies, so the fix is shown to be the thing that keeps it alive |
| `sdlocktest.sh` (not in `runtests`) | [`scripts/sdlocktest.sh`](../../scripts/sdlocktest.sh): boots the QEMU `raspi4b` build with a given image as the SD card and checks the write-grant verdict (granted for an f2fs volume labelled `leandros-root`, locked with a stated reason for anything else) |
| `usbprofile-test.sh`, `scripts/check-usbroot-nosdhci.sh` (not in `runtests`) | The Pi 5 USB-root profile builds without the SD driver and its stick image writer and tryboot staging tools work |

**Running the whole regress faster.** `runtests.py --shards K` splits the command list over K concurrent runs, longest first by the durations recorded from earlier runs (`$RUNTESTS_DURATIONS`, default under `~/.cache/leandros/`), each with its own VM, run id and reflink copy of the images, and then merges them into the same `=== ` lines and one summary; every command still runs, and the overall result is PASS only if every shard passes. Self-booting host suites each get a private fsynced clone of the tree's boot image, root image and UEFI variables: before that, QEMU's image write locks made concurrent suites die instantly with no output. Their output is captured to `logs/<suite>.log`. A full x86-64 regress including the build took about 1030 s sharded against about 2600 s before.

**Harness behaviour worth knowing before trusting a result.**

- *Long command lines are staged.* brush repaints the whole edit line for every input burst, so typing an N-byte line costs O(N^2) serial output. Lines longer than `LEANDROS_LONG_LINE` (default 600) are written to a guest file in 300-byte typed pieces and run with `.`; `raw` mode (the `rawline` suite) never stages. Before this, a 3.7 KB line was cancelled and retried until the suite timed out with the guest idle.
- *The prompt is found under kernel log lines.* A prompt glued to a timestamped kernel line (`[   26.096999] [USB-MSC] ...`, any tag, any case) used to hide it, so the driver waited out a 3 s timeout per command; that looked like an echo stall on x86-64 and was the harness.
- *Arch-only host suites skip* on the other architecture instead of failing, and `runtests` re-boots its VM after a host script that booted its own.
- *Lines the harness parses are written with one `write()` each* (init's status and reboot-request lines), so another writer cannot splice into them. Splicing was the real cause of two sightings once read as lost output: an aarch64 diagnostic that bypassed the console lock ("lost console line") and long x86-64 console lines, which are now staged and written whole up to 2048 bytes.
- *Output is line-buffered* and the box farm's runs are detached, so a dead waiter loses nothing (see [workflow](workflow.md#locks-the-regress-farm-and-lane-discipline)).
- `ffsession.py --noterm` skips the cosmic-term step, which the session tests (`blanktest`, `portalshot`, the virgl suites) do not need and which was their main flake; `ffsession` runs under an inner time budget and prints `DIAG` lines (serial tail, `steps.json`) when it is killed, so a timeout carries its own evidence.

Known flakes (re-run once under the lock before investigating): the `nettag2test` wake-count cases, `sigtest timed_wait_stop_resumes_remainder`, `timertest realtime_is_not_uptime`, `scmtest tcp_connect_refused`, `sshsmoke ssh_hostkey`. A wedged `socket_vmnet` bridge (one stalled VM deafens the others) makes `runtests` fall back to SLIRP.

The graphical session has its own tests

`portalshottest.py <arch> [tag]` (suite `portalshot`, also in `regress`) boots the same session, then as `leandro` runs `leandros-sysbus portal screenshot 90`: it grants the PermissionStore "screenshot" permission (otherwise the portal waits on an Access dialog nobody answers) and requires Screenshot response 0 and a non-empty PNG. About 1.5 min per arch.

The graphical session has its own tests outside `runtests.py`, because `regress` boots without a compositor. `blanktest.py <arch> [tag]` boots a GPU session (`ffsession.py --nofirefox`) and runs `/bin/blanktest` against cosmic-comp. The test powers every output off and checks four things: frame callbacks keep coming at about 1/s, a window mapped while the outputs are off gets its first callback, an idle inhibitor on such a window does not resume idle, and the compositor leaves the outputs off ([ports/cosmic-comp](../../ports/cosmic-comp/README.md)).

A command `host:<script> [args]` runs a script from the skill directory on the host. Output goes to `$FFSESSION_OUT/tests-<tag>/` (default `/tmp/ffsession`): every command's output, the serial log, QEMU's full serial log and `summary.json`; the exit status is 0 only if every command returned 0 with a known status.

For a single long command by hand, [`scripts/scmrun.py`](../../scripts/scmrun.py) keeps one serial connection open and reads until an optional completion marker. A persistent reader is necessary: QEMU drops serial output while nothing is attached, and a long test run then looks like a hang. Build the marker so that it does not appear in the command line itself — the tty echoes the command, and a marker matched from the echo ends the read before the test has printed anything (`echo "M13""RC=$?"` is typed as `M13""RC=` and printed as `M13RC=`).

Integration verification of a wave runs the full battery on both architectures — every suite above, a graphical session on each GPU path, VT switching, power-off and restart, `privtest` as root and as `leandro` — and checks the serial logs for `panic`, `[WDOG]`, `[PF]`/SEGV (other than the suites' intentional faults) and `[BUDDY]` lines ([workflow](workflow.md#lanes-and-waves)).

## Fresh images

Run the suites against a **fresh disk image** when the result matters. Images persist across boots, and a suite that has already run against one can leave state that changes the next run's outcome — `O_TRUNC` clears a file's data but not its xattrs, which is exactly the sort of thing that manufactures a phantom architecture-specific failure. `vfstest` in particular leaves residue (`/tmp/jail`, xattr test files) and should run once per fresh image.

A rebuild (`./scripts/build-all.sh --arch <arch>`) writes a fresh root (`f2fs-data0`). It does **not** touch `f2fs-data1-<arch>.img`, the persistent `/home` volume: that is the user's data. Tests do not dirty it either: `driver.py start` boots a throwaway copy-on-write clone of it (`f2fs-data1-<arch>.run-<RUN_ID>.img`, replaced at every start, removed by `stop`), so concurrent runs never share one writable home image and nothing a suite writes there survives the run. `LEANDROS_PERSIST_HOME=1` boots the persistent image itself, for checking that something persists. The f2fs cases of `vfstest`, `permtest` and `exectest` run in their own directories on that volume (`/home/.vfstest`, `/home/.permtest`, `/home/.exectest-redir`), so they still cross a mount point as they did on `/data`. For parallel runs, clone the root copy-on-write rather than sharing one ([workflow](workflow.md#build-outputs-and-image-provenance)).

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
