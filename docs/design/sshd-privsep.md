# sshd privilege separation

Status: P1-P4 implemented (listener, per-connection `sshd -R`, monitor/net-child split,
seccomp allow-list), plus the listener backstop for a stalled monitor and `close_range`
before the id drop. Measured numbers are under "Cost measurements".

## Process model

```
sshd (listener, root)            accepts TCP, MaxStartups, LoginGraceTime kill
 `- sshd -R (per connection)     monitor, root: host key, auth, sessions
     `- sshd-preauth (net child) uid/gid 22, chroot /var/empty, NNP, seccomp
```

- The listener (`userland/sshd/src/main.rs`) only accepts. It re-execs `sshd -R` for each
  connection (socket on fd 3, startup pipe on fd 4), so a crash costs one connection.
  It enforces `MaxStartups start:rate:full` (the slot is released when the child signals
  authentication or exits) and kills a child still unauthenticated after `LoginGraceTime`.
- The monitor (`monitor.rs`) is root, single-threaded, nonblocking, with deadlines. A child
  that stalls, speaks out of turn or attaches unexpected fds is killed.
- The net child (`netchild.rs`, comm `sshd-preauth`) runs russh (kex, packet parsing, auth
  state machine, channels, forward I/O). It holds no secret.
- Monitor and child talk over two SEQPACKET socketpairs, framed by `rpc.rs`: `sign` (host-key
  signatures only) and `ctl` (everything else, fds passed by SCM_RIGHTS), plus a log pipe.

## What stays in the monitor

- **Host-key signing.** The child gets only the public key and a stand-in `PrivateKey`
  with a zero private half (`stand_in`); russh's `host_key_signer` hook sends
  `SIGN(alg, hash, blob, digest)` on `sign` and the monitor signs. Only ed25519 host keys work
  under privsep. No host-key fd exists in the child.
- **Authentication checks.** Password verify (`/etc/shadow`, `$sha256$` rehash), authorized_keys
  lookup and `KeyOpts` parsing, and the raw publickey signature re-verify against the kex
  session id (`userauth.rs`) all run in the monitor; the child only relays attempts and
  receives the verdict.
- **Session setup.** pty open, fork+exec as the user (setgroups/setresgid/setresuid), utmp,
  cgroup scope (`cgsession.rs`), exec/subsystem/sftp, and opening `-L`/`-R` forward
  sockets (including privileged-port binds) are monitor operations; the child receives the
  resulting fds (pty master, pipes, accepted sockets) and only does channel I/O.

## Sandbox of the net child

`sandbox::confine(22, 22, "/var/empty", keep)` (`sandbox.rs`), in this order:
1. close every fd from 3 to 1023 except tcp, sign, ctl and the log fd;
2. `chroot("/var/empty")`, `chdir("/")` (needs root, so before the id drop);
3. `setgroups(0)`, `setresgid`, `setresuid` to 22, read back, and prove `setuid(0)` fails;
4. `PR_SET_NO_NEW_PRIVS`, `PR_SET_DUMPABLE 0`, `RLIMIT_CORE 0`.

Then `sandbox::install_seccomp` installs the filter. The allow-list is the `COMMON` table in
`userland/sshd/src/sandbox.rs` (names with x86_64 and aarch64 numbers; x86_64 also allows
`poll`). It covers socket I/O, epoll/ppoll/eventfd, futex, clocks, memory (mmap family,
`mprotect` without PROT_EXEC), signals, `getrandom`, exit, `fcntl`, `socketpair`, and
`mlock`/`munlock` (russh-cryptovec). `prctl` is limited to `PR_SET_NAME`; `ioctl` to
`FIONBIO`, `FIONREAD`, `TIOCSWINSZ`. Anything else, or a wrong audit arch, is
`SECCOMP_RET_KILL_PROCESS`, which ends only that connection. The `/var/empty` directory and
uid 22 come from the image builder (`docs/servers/security.md`).

## Debugging

- `SSHD_SECCOMP=log` in the sshd environment makes the filter log (`SECCOMP_RET_LOG`) instead
  of kill, for finding a missing syscall; a wrong arch still kills. Add the syscall to
  `COMMON` (both numbers) and re-run the unit test.
- `SSHD_TEST_HOOK=badsyscall` makes the child issue a forbidden `socket()` after the filter
  (test only).
- Per-connection messages go to the sshd log with the peer address (`cannot confine ...`,
  `handshake failed`, `still unauthenticated after LoginGraceTime`, `dropping connection (MaxStartups)`).
- Inside the guest, `comm` is `sshd-preauth` for the child; `/proc/<pid>/status` shows `Seccomp: 2`,
  `NoNewPrivs: 1`, and `/proc/<pid>/root` is `/var/empty`.
- Tests: host unit crate `userland/sshd/unit` (filter checked with the seccomp-bpf interpreter
  for both arches: allow-list, wrong arch, argument filters); `.claude/skills/run-leandros/sshsmoke.py`
  (runtests suites `ssh`, `sshfull`, and part of `regress`) with steps `privsep_preauth_uid`,
  `privsep_preauth_chroot_nnp`, `privsep_preauth_seccomp`, `privsep_no_hostkey_fd`,
  `privsep_postauth_uid`, `privsep_hooks` (incl. `privsep_forbidden_syscall_kills_conn`,
  `privsep_forged_auth_refused`), `privsep_grace`/`privsep_grace_kill`, `privsep_crash_isolated`
  (probe_eof is KNOWN-OPEN, not gating), `privsep_count`, `privsep_listener_survives_count`.

## Stalled monitor and descriptor hygiene

- While a connection is unauthenticated the listener arms a backstop of `LoginGraceTime + 10 s`;
  if the monitor (which normally enforces the grace time itself) is stopped or wedged, the
  listener kills the `sshd -R` process and the client sees EOF. Test `privsep_stalled_monitor_backstop`
  (SIGSTOP of the monitor only).
- `sandbox::close_except` closes every fd from 3 up with `close_range(2)` (loop to RLIMIT_NOFILE
  if the syscall is missing) in the net child before chroot/id drop and in the forward-socket
  fork, so descriptors above 1023 cannot leak. Test `privsep_confine_closes_high_fds`
  (hook `SSHD_TEST_HOOK=highfd` opens /etc/shadow at fd 1100 in the monitor; it was found in the
  net child before the fix).

## Cost measurements

`sshsmoke` step `privsep_perf_measured` (QEMU guest, ssh from the host over the forwarded port,
4 authenticated sessions open; 2026-11-17, box). RSS in kB per process:

| arch | listener | monitor/session | net child (sshd-net) |
|---|---|---|---|
| aarch64 (TCG) | 2164 | 2008-2108 | 2920 |
| x86_64 (KVM) | 2404 | 2212-2360 | 3244-3268 |

So one connection costs about 5-6 MB RSS (monitor + net child), before the user's shell.
Throughput of 16 MiB through `ssh host cat`, and scp (sftp mode; the guest has no `scp` binary):

| arch | ssh pipe up | ssh pipe down | scp up |
|---|---|---|---|
| aarch64 (TCG) | 3.7 MiB/s | 12.6 MiB/s | 4.5 MiB/s |
| x86_64 (KVM) | 55.9 MiB/s | 84.5 MiB/s | 13.2 MiB/s |

The aarch64 numbers are dominated by TCG emulation of the crypto, not by privsep.

## Original design

### Target shape (as OpenSSH)

1. **Listener (root, tiny).** Accepts the TCP connection, forks one monitor per
   connection, owns host keys and `/etc/shadow` access.
2. **Pre-auth child (uid `sshd`, chroot `/var/empty`, seccomp).** Runs the russh
   transport and the auth state machine. It cannot read `/etc/shadow`, host
   private keys or `authorized_keys`; it asks the monitor over a socketpair:
   `sign(hash)` (host key), `verify_password(user, pw)`, `lookup_key(user, key)`
   returning the parsed `KeyOpts`, and `rehash(user, pw)`.
3. **Post-auth.** The monitor forks the session processes as the user (as today);
   the transport child switches to the user's uid, keeping only the channel and
   forwarding I/O, so `-L`/`-R` sockets are opened by an unprivileged process.

## Pieces needed

- an `sshd` system user and `/var/empty` (image builder, `docs/servers/security.md`);
- a russh `Signer`/key-agent style hook so the host key never enters the child
  (russh 0.64 takes a `PrivateKey` in `Config`; signing must go through a trait or
  the child gets a verify-only stub plus monitor RPC);
- a small length-prefixed RPC between monitor and child (`serde`-free, fixed enums);
- seccomp-bpf profile via `lib/seccomp-bpf` for the pre-auth child;
- tests: `sshsmoke` plus a case that the pre-auth process has uid != 0 and cannot
  open `/etc/shadow`.

## Cost

Estimated 3-5 days; the RPC and the russh signer hook are the risky parts.
