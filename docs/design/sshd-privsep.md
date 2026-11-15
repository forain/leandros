# sshd privilege separation (design note, not implemented)

Today `/usr/sbin/sshd` runs as root in one process: the listener, the russh
transport (key exchange, packet parsing, authentication) and every connection's
task share an address space. Only the session children drop privilege (setgroups,
setresgid, setresuid, then exec). A memory-safety or logic bug in the unauthenticated
protocol code therefore runs as root.

## Target shape (as OpenSSH)

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
