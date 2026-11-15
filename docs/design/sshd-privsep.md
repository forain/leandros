# sshd privilege separation

Status: P1-P4 implemented (listener, per-connection `sshd -R`, monitor/net-child split,
seccomp allow-list). Open: RSS/throughput numbers, a listener backstop for a stalled
monitor, `close_range` before the id drop (see `TODO.md`).

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
