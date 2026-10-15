# lane/gitssh: git in the base image, and an SSH server on russh (2026-10-14/15)

Branch `lane/gitssh` off main 9f4f1753. Worktree `.claude/worktrees/gitssh`, run ids
`gitssh` (aarch64) and `gitsshx` (x86_64). User request: "include git as userland. Also
include a Russh SSH server that starts on boot and listens to all network interfaces by
default".

## How git ships, and why this way

Three conventions were available for shipping C programs (docs/userland/ports.md):
1. Cross-building from source against musl.
2. The Alpine-chroot launcher (`/bin/alpine-run`, which serves claude-code).
3. Repackaging Alpine 3.21 binaries into the image, as Firefox, PipeWire and the portal do.

git uses the third, as `ports/git`. The reasons:

- **The same binaries run on Linux.** No git, curl or OpenSSL build needs maintaining.
  `git-remote-https` comes for free.
- **Repackaged binaries run against the guest's own libc and paths.** git is then on the
  normal PATH for every user and in every context: console, VTs, cosmic-term, and ssh exec.
  git-over-ssh needs exactly that.
- **The chroot launcher was ruled out.** It needs a separate install step
  (`claude-code-install`), mounts and a setuid launcher. A server-side `git-upload-pack`
  could not run through it transparently.
- **Cross-building from source was ruled out** because of the dependency stack: curl,
  OpenSSL, pcre2 and expat. It would work, but it is the most code to keep up for no gain.

**What the port stages** (`ports/git/build-in-alpine.sh`, run in a container or in the
apk.static chroot):
- git 2.47.3 with `/usr/libexec/git-core`;
- the templates (they come from the git-init-template subpackage);
- less (the pager);
- nano, which `/etc/gitconfig` sets as `core.editor` (the image had no `vi`);
- OpenSSH 9.9's `/usr/lib/ssh/sftp-server`, used by sshd;
- `/etc/terminfo`, because the image had no terminfo for less and nano;
- the DT_NEEDED closure: libcurl, OpenSSL 3, nghttp2, c-ares, idn2, psl, brotli, pcre2,
  ncursesw and the rest.

The ELFs are fixed up exactly like PipeWire's: `libc.musl-*` is rewritten to `libc.so`, and
binaries that need `__stack_chk_guard` get `libleandros_ssp.so.1`. Sonames the image already
ships keep the image's copy (zlib, expat, zstd, pcre2, ...).

**mkfs changes.**
- Every symlink inside a port tree is now packed as a hard link to its target. git's ~150
  `libexec/git-core/git-*` links point at `../../bin/git`, and they cost one inode instead
  of 150 copies of 3 MB.
- `git`, `git-upload-pack`, `git-receive-pack`, `git-upload-archive`, `less` and `nano` also
  get `/bin` names, as hard links to the same inodes. `/bin/login` gives `PATH=/bin`, so
  without these names console users would not find git.
- HTTPS uses the CA bundle that ports/apk already ships at
  `/etc/ssl/certs/ca-certificates.crt`, which is Alpine curl's default path.

## sshd architecture

The daemon is `userland/sshd`, a std/musl crate like `claude-run`, with its own `build.sh`.
- **Libraries.** russh 0.64.1 uses `ring` instead of the default `aws-lc-rs`. ring's C and
  assembly build through the zig cc/ar wrappers, while aws-lc-rs would need cmake and a C++
  cross toolchain. It also enables `flate2` and `rsa`, so ssh-rsa client keys work. tokio
  runs multi-threaded with 2 workers.
- **One root process** serves every connection, built on `russh::server::run_stream` with
  its own accept loop. Each session is a child process. Between fork and exec the child calls
  `setsid`, then (pty only) `TIOCSCTTY`, then setgroups (from `/etc/group`), setresgid and
  setresuid. It reads the uid, euid, gid and egid back, checks that `setuid(0)` fails, and
  changes to the home directory as the user. This is the same rule as greeter-launch and
  claude-run.
  - The parent never drops privilege. A russh bug is therefore a root bug, the same trade
    as OpenSSH without privilege separation. See "security caveats".
- **Interactive shells** get a pty from `/dev/ptmx` (posix_openpt, grantpt, unlockpt,
  ptsname_r). The slave is fchowned to the user with mode 0620; this is best effort. The
  master is non-blocking and registered with tokio's `AsyncFd`.
  - The shell runs as a non-login shell with `argv[0]` set to its path, exactly as
    `/bin/login` starts it, so ssh and the console give the same environment.
  - When the shell exits, the pty is drained for up to 500 ms. A background job holding the
    tty cannot keep the channel open.
- **Exec requests** run `$SHELL -c cmd` on three pipes, with stderr sent as extended data 1.
  Client EOF closes the child's stdin. When the child exits, sshd waits up to 5 s for its
  stdout and stderr to reach EOF.
- **The sftp subsystem** execs `/usr/lib/ssh/sftp-server` directly, never with a pty.
  OpenSSH's `scp` uses SFTP since 9.0, so scp works too.
- **Other requests.**
  - `window-change` → `TIOCSWINSZ`.
  - `signal` → `kill(-pgid)`.
  - Exit codes → `exit-status`; deaths by signal → `exit-signal`; then EOF and close.
  - When a client disconnects or closes the channel, the session's process group gets
    `SIGHUP`.
- **Environment.**
  - `HOME`, `USER`, `LOGNAME`, `SHELL`;
  - `PATH`: `/usr/local/bin:/usr/bin:/bin`, with the sbin directories added for root;
  - `XDG_RUNTIME_DIR=/run/user/<uid>`;
  - `SSH_CLIENT`, `SSH_CONNECTION`, `SSH_TTY`, and `TERM` from the pty request;
  - from `env` requests, only `LANG`, `LC_*`, `COLORTERM` and `TZ` (Debian's AcceptEnv).
- **russh configuration.**
  - `inactivity_timeout` is off: russh's 600 s default would disconnect an idle shell.
    Instead, 60 s keepalives drop the connection after 3 misses.
  - TCP_NODELAY is set.
  - Auth rejections take 1 s, with no delay on the initial "none" probe.
  - LoginGraceTime 120 s is enforced with a timer that disconnects unauthenticated
    connections.

### Authentication

- **Passwords.**
  - The `/etc/shadow` `$sha256$<salt>$<hex>` scheme is the one `/bin/login` and
    pam-leandros use, compared in constant time.
  - `PermitEmptyPasswords` is always no.
  - Accounts whose shadow field starts with `!` (locked) are refused by every method.
  - Accounts with a missing, `false` or `nologin` shell are refused, which keeps out
    `cosmic-greeter`.
- **Public keys.** `~/.ssh/authorized_keys`, or `AuthorizedKeysFile`, which accepts `%u` and
  `%h`. OpenSSH's StrictModes applies: the file and each directory up to the home must be
  owned by the user or root and not writable by group or others. Lines with options
  (`command=`, `from=`, ...) are skipped rather than honoured, so a restricted key never
  gets an unrestricted login.
- **root.** `PermitRootLogin prohibit-password`, the OpenSSH default: root may log in with a
  key in `/root/.ssh/authorized_keys`, never with a password. The image ships the
  well-known development password `root/root`, and sshd listens on every interface.
- **Methods offered.** `publickey` and `password`; no keyboard-interactive. OpenSSH clients
  fall back to `password` by themselves.

### Configuration and boot

- **`/etc/ssh/sshd_config`** (from `userland/sshd/sshd_config`) accepts an OpenSSH keyword
  subset: `Port`, `ListenAddress`, `HostKey`, `PermitRootLogin`, `PasswordAuthentication`,
  `PubkeyAuthentication`, `PermitEmptyPasswords no`, `AuthorizedKeysFile`, `MaxAuthTries`,
  `LoginGraceTime`, `Subsystem sftp`. Unknown keywords are logged and ignored.
  - `sshd -t` checks the file and the key; `-e` logs to stderr; `-f` names another config;
    `-p` overrides the port.
- **Listening.** With no `ListenAddress` lines, sshd listens on `0.0.0.0:22` and tries
  `[::]:22`. The latter fails with `EAFNOSUPPORT` (97), because LeandrOS has no IPv6 yet; the
  failure is logged and skipped.
- **Host key.** It is generated on first boot from 32 bytes of `getrandom` and written
  atomically to `/etc/ssh/ssh_host_ed25519_key` (0600, plus `.pub` at 0644). It is not
  created at image build time, so no two images share a key. Rebuilding the image does
  generate a new key, and the host's `known_hosts` then warns.
- **Log.** `/var/log/sshd.log` (0600).
- **Start-up.** init starts sshd after the autotests, before the first login prompt, in its
  own session with stdin on `/dev/null`.
  - Opt out with `/etc/ssh/sshd_not_to_be_run` (Debian's file) or `sshd=0` on the kernel
    command line.
  - init restarts it after a 1, 2, 4, 8 or 16 s backoff, and gives up after five exits
    within 60 s.
  - `sweep_strays` spares it, and the memory guard takes it only as a last resort.

## QEMU / harness

- **Port forwarding on SLIRP.** `run-qemu.sh` and `driver.py` add
  `hostfwd=tcp:127.0.0.1:<port>-:22`.
  - The port is 2222 with no run id. With a run id, it is in 2223..2999, derived from
    (tree, arch, run id).
  - Either runner then takes the first free port from there, because QEMU refuses to start
    when a hostfwd cannot bind. `LEANDROS_SSH_PORT` pins the port.
  - `driver.py ssh-target` prints `host port`. Under socket_vmnet it prints the guest's
    DHCP address from the serial log, with port 22, since vmnet is directly reachable.
- **`sshsmoke.py`** (in the skill directory) uses the Mac's real OpenSSH client, ssh-keyscan,
  sftp, scp and git. Passwords go through `SSH_ASKPASS` with `SSH_ASKPASS_REQUIRE=force`, so
  no sshpass is needed.
  - Interactive sessions run under a Python pty that answers brush's cursor-position queries
    (`ESC[6n`), as a real terminal does. Without the answers, reedline gives up with "cursor
    position could not be read".
- **`runtests.py`** runs a command written `host:<script> [args]` on the host. The regress
  suite now ends with `host:sshsmoke.py`. New suites: `ssh` (the quick pass) and `sshfull`.

## Kernel fix

- **close(0/1/2) of the implicit serial console returned EBADF** (adbcb9c5).
  - **Mechanism.** The console has no fd-table entry: an unused slot 0..2 *is* the console.
    `close(1)` therefore reached `handle_close`, found no entry and answered -9.
  - **Effect on git.** At the end of every builtin, git calls `fstat(1)`. That reported a
    character device, the implicit console. git then called `fclose(stdout)`, which failed,
    and git printed `fatal: close failed on standard output: Bad file descriptor`, exit
    128.
  - **Where it appeared.** At the serial login, and in any git child that had closed its
    stdout: a local `git push` (receive-pack) and `upload-pack` over ssh. The git clone over
    ssh then aborted with "possible repository corruption on the remote side".
  - **Fix.** `close()` of an unredirected 0..2 now returns 0 after dropping its epoll
    registrations.
  - **Remaining gap.** There is no "closed console" state, so a later write still reaches
    the console instead of failing with EBADF, and `fstat` of an explicitly closed 0..2 still
    reports the console. Linux answers EBADF for both.

Nothing else in the kernel needed changing. The pieces russh and tokio rely on already held
up: epoll on inet sockets and on the pty master, pidfd child reaping, `getrandom`, privileged
bind as root, TCP_NODELAY and `TIOCSWINSZ`.

## Results (fresh images, slirp, 2026-10-15)

| | aarch64 (HVF), run id gitssh, :2338 | x86_64 (TCG), run id gitsshx, :2692 |
|---|---|---|
| regress + ptytest + privtest (+ host:sshsmoke) | 23/23 PASS | 23/23 PASS |
| `sshsmoke.py --full` (Mac OpenSSH client) | 19/19 | 19/19 |
| `ssh -p P leandro@localhost uname -a` | `Leandros leandros 1.0.0 #1 aarch64 Linux` | `... x86_64 Linux` |
| pty shell, `stty size` 30x100 -> window change -> 41x132 | yes | yes |
| key login (leandro, and root by key only) | leandro | leandro + root |
| root password | refused | refused |
| sftp put/get, scp both ways (300 KB, byte-identical) | yes | yes |
| git in guest as leandro: version/init/add/commit/log | ssh exec + serial login | ssh exec + serial login |
| `git clone https://github.com/octocat/Hello-World.git` in guest | yes | yes |
| `git clone ssh://leandro@127.0.0.1:P/home/leandro/srv.git` from the Mac, then push | yes | yes |
| host key identical after reboot (ssh-keyscan) | yes | yes |

These were measured, not inferred. One thing is untested: an interactive login from a real
terminal emulator on the Mac. The pty tests use a Python pty as the client terminal, running
the real ssh binary.

## Security caveats / open items

- **IPv6:** none in the kernel (`EAFNOSUPPORT`). The `::` listener is skipped and logged. It
  should start working without changes once AF_INET6 exists.
- **No privilege separation:** the protocol runs as root in one process (see above).
- **Weak password hash:** passwords use the system's single salted SHA-256 pass, not a
  slow KDF. The development accounts (`leandro/leandro`) are reachable from the network
  wherever port 22 is.
  - On SLIRP that is only the host's loopback.
  - Under socket_vmnet or on real hardware it is the LAN.
  - root is key-only.
- **Missing OpenSSH features:** no `authorized_keys` options, no keyboard-interactive, no
  port forwarding (direct-tcpip and tcpip-forward are refused), no agent or X11 forwarding,
  no `MaxStartups` / per-source rate limiting, no utmp/wtmp or lastlog.
- **The pty slave is chowned best effort.** The kernel does not record pty ownership (see
  lane claudecode).
- **`ssh host` gives a non-login shell,** like the console login, so `/etc/profile` is not
  read.
- **The host key changes whenever the image is regenerated** (first boot generates it).
- **brush needs a terminal that answers DSR (`ESC[6n`).** Every real terminal does, but a
  dumb client pty does not, and the prompt then falls back to an error.
