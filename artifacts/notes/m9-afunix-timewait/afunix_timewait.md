# M9 — AF_UNIX strict `listen()` (TODO 10) and TCP TIME_WAIT (TODO 11)

Lane F. Worktree based on `a0f2c46`. Two patches, prepared but **not** landed on `main`:

- `afunix_listen_strict.patch` — TODO item 10
- `tcp_time_wait.patch` — TODO item 11

Both touch only `servers/net/src/lib.rs` and `userland/scmtest/src/main.rs`. Neither
touches `drivers/` or `servers/vfs`, so there is no overlap with the other lanes of
this wave. Neither touches `TODO.md` — items 10 and 11 are adjacent there and editing
it would be the one place the two patches could conflict.

Verified independent: each applies cleanly to bare `a0f2c46`, and they apply cleanly
in **either order** on top of each other (`git apply --check` both ways). Their hunks
are disjoint — item 10 touches `handle_listen` only; item 11 touches `handle_bind`,
`handle_connect`, `handle_close`, `alloc_ephemeral_port`, `SockEntry`, the dispatch
table and a new `handle_setsockopt`.

---

## 1. Ground truth: what Linux actually does

### `unix_listen()` — net/unix/af_unix.c (torvalds/master, fetched 2026-08-06)

```c
static int unix_listen(struct socket *sock, int backlog)
{
	...
	err = -EOPNOTSUPP;
	if (sock->type != SOCK_STREAM && sock->type != SOCK_SEQPACKET)
		goto out;	/* Only stream/seqpacket sockets accept */
	err = -EINVAL;
	if (!READ_ONCE(u->addr))
		goto out;	/* No listens on an unbound socket */
	err = prepare_peercred(&peercred);
	if (err)
		goto out;
	unix_state_lock(sk);
	err = -EINVAL;
	if (sk->sk_state != TCP_CLOSE && sk->sk_state != TCP_LISTEN)
		goto out_unlock;
	if (backlog > sk->sk_max_ack_backlog)
		wake_up_interruptible_all(&u->peer_wait);
	sk->sk_max_ack_backlog	= backlog;
	WRITE_ONCE(sk->sk_state, TCP_LISTEN);
	...
	err = 0;
```

Three gates, **in this order**:

1. type is not STREAM/SEQPACKET → **EOPNOTSUPP (95)**, checked *before* the address,
   so even a bound DGRAM socket answers 95 and not 22.
2. `u->addr == NULL` (never bound, never autobound) → **EINVAL (22)**.
3. `sk_state` is neither `TCP_CLOSE` nor `TCP_LISTEN` → **EINVAL (22)**.

`unix_stream_connect()` sets `sk->sk_state = TCP_ESTABLISHED` on the connector before
it returns 0. AF_UNIX stream connect never reports EINPROGRESS: it either completes
(ESTABLISHED) or fails, and on `-EAGAIN` (peer backlog full, O_NONBLOCK) the socket is
left `TCP_CLOSE`. **There is no persistent "connect in progress" state on Linux.**

For contrast, `inet_listen()` answers **EINVAL** for a DGRAM listen where AF_UNIX
answers **EOPNOTSUPP**. The two are not symmetric and must not be made so; the AF_INET
arm (landed as `07d461c`) is left alone.

### The five-state table

| Linux state | LeandrOS `SockState` | Linux answer | HEAD answer | after patch |
|---|---|---|---|---|
| unbound (`!u->addr`), TCP_CLOSE | `Unbound { .. }` | EINVAL 22 | **0** | EINVAL 22 |
| bound, TCP_CLOSE (not yet listening) | `UnixListening { .. }` | 0 | 0 | 0 |
| bound, TCP_LISTEN (repeat listen) | `UnixListening { .. }` | 0 (updates backlog only) | 0 | 0 |
| connected, TCP_ESTABLISHED | `UnixConnected { .. }` | EINVAL 22 | **0** | EINVAL 22 |
| connect in progress | *does not exist on Linux* — see below | — | — | — |
| — plus: type not STREAM/SEQPACKET | any | EOPNOTSUPP 95 | **0** | EOPNOTSUPP 95 |

Two rows need explaining.

**Rows 2 and 3 are one state here.** `handle_bind` is what marks an AF_UNIX socket
`UnixListening` — `listen()` has never done any arming — so this server cannot tell
"bound, still TCP_CLOSE" apart from "already TCP_LISTEN". Linux answers 0 to *both*,
so collapsing them onto a single success arm is faithful and keeps `listen()`
idempotent. Nothing is re-armed on the repeat call; re-running any of bind's work
would be the same orphaned-listener bug `07d461c` avoided on the AF_INET side.

**The fifth state maps onto ESTABLISHED, not onto anything new.** LeandrOS has a
`UnixPendingAccept` state that Linux has no analogue for: our `connect()` returns 0
immediately and the pairing happens at `accept()`. But by the time userspace could
call `listen()` on such an fd, `connect()` has already returned 0 — and on Linux a
`connect()` that returned 0 means TCP_ESTABLISHED. So `UnixPendingAccept` is EINVAL,
same as `UnixConnected`. There is no fifth answer to give.

`SockState::None` and the `Inet*`/`Icmp*` states are unreachable on an AF_UNIX fd
under normal use, but a `bind()` handed a `sockaddr_in` on an AF_UNIX fd takes the
AF_INET arm of `handle_bind` and can leave `InetBound` there; EINVAL is right for
that too. All of them fall into the same `_` arm.

---

## 2. Item 10 blast radius

The patch is a **no-op for every healthy server**: the only state a working AF_UNIX
server is in when it calls `listen()` is `UnixListening`, and that arm still answers 0
for any number of calls. All risk is concentrated on paths that are already broken.

### Call shapes that change from success to error

**(1) `listen()` on a never-bound AF_UNIX socket → 0 becomes EINVAL.**

The only realistic way to reach this is a server whose `bind()` failed and which
*ignored the return value*. On LeandrOS `bind()` can fail with:

- `EADDRINUSE` — a stale `S_IFSOCK` node from a previous session. **This is the one
  that actually happens**: `/data` survives reboots, sockets under it outlive their
  listeners (as on Linux), and the dirty-image failure mode is already documented
  elsewhere in this project's notes.
- `ENOENT` / `EACCES` — parent directory missing or unwritable.
- `EOPNOTSUPP` — binding off a tmpfs.
- `ENOMEM` — `BOUND_PATHS` full (`MAX_BOUND = 512`).

Today such a server gets `listen() == 0` and then `accept()` returning EAGAIN forever:
it looks alive and serves nobody. After the patch it gets EINVAL at `listen()` and
normally exits. **The state was already broken; what changes is whether it fails
loudly or silently.** The realistic bad outcome is a component that today limps on as
a zombie and after the patch exits and gets restarted by `launch_pad` in a loop —
which reads as a regression in a session log even though the underlying fault is
older. That is the single reason this needs a live session.

Rust `std`'s `UnixListener::bind` and tokio's propagate the bind error and never reach
`listen()` with an unbound socket, so the ordinary path is not exposed at all.

Socket-activation style code that `listen()`s on an *inherited* fd is safe: an
inherited AF_UNIX listener is already `UnixListening` (`handle_fork_dup` /
`handle_exec_cloexec` preserve the state), so it takes the success arm.

**(2) `listen()` on a socketpair end, an accepted socket, or a connector →
0 becomes EINVAL.** No sane code does this. No in-tree caller does.

**(3) `listen()` on an AF_UNIX SOCK_DGRAM socket → 0 becomes EOPNOTSUPP (95).**
This arm fires regardless of state, so in principle it has the widest reach — but it
requires creating a DGRAM unix socket and then calling `listen()` on it, which is
meaningless (`accept()` on it already fails). It is a single `if` and can be dropped
independently if any doubt appears; the rest of the patch does not depend on it.

**(4) An AF_UNIX fd left in an `Inet*` state by a mismatched `bind()`.** Pathological.

### What does not change

`socket → bind → listen` (any number of times) still returns 0. `accept()`, `bind()`,
`connect()` and the whole AF_INET arm of `handle_listen` are untouched. Abstract and
pathname listeners behave identically (bind sets `UnixListening` for both).

### In-tree audit

Every `listen()` caller in the tree binds first and checks the result:

- `userland/scmtest/src/main.rs` — 4 call sites (`test_socket_node_roundtrip`,
  `test_unlink_rebind` ×2, `test_many_socketpairs_and_listeners`).
- `userland/wakepolltest/src/main.rs` — `bind_listen_abstract`, which returns -1 if
  `bind` fails and never calls `listen`.

There are no other `listen()` callers in `userland/`, `servers/`, `kernel/` or `lib/`.
Everything else that listens (cosmic-comp, cosmic-panel, busd, tokio/zbus) is
out-of-tree and goes through the std/tokio bind-then-listen wrappers.

---

## 3. Item 11: what TIME_WAIT actually got

### Implemented

- A **60 s port reservation**, matching Linux's `TCP_TIMEWAIT_LEN` (2*MSL with
  MSL = 30 s; not tunable there either). Expressed as `60 * 100` ticks, since
  `sched::ticks()` runs at 100 Hz.
- Recorded **only for the active closer of a connection that reached Established**.
  smoltcp's state at close decides: `Established | FinWait1 | FinWait2 | Closing |
  TimeWait` park; `CloseWait`/`LastAck` mean the peer's FIN already arrived (the
  passive close, which goes straight to CLOSED on Linux) and do not; `Closed`,
  `Listen`, `SynSent`, `SynReceived` never established and do not.
- The local port is read back from smoltcp's `local_endpoint()`, **not** from
  `bound_port` — `handle_accept` leaves `bound_port == 0` on the accepted socket, and
  the accepted socket sharing the listener's port is exactly what makes a restarted
  server fail to rebind on Linux.
- `bind()` to an explicit reserved port → **EADDRINUSE (98)**, unless SO_REUSEADDR.
- `alloc_ephemeral_port()` skips reserved ports, for both bind-to-0 and `connect()`'s
  source port. This is unconditional — the caller asked for "any free port", not for
  that one.
- **SO_REUSEADDR made real.** `NET_SETSOCKOPT` was a bare `ok_reply()` with the args
  not even forwarded. Without recording the flag, adding TIME_WAIT would *break the
  very restart it models*: a real server sets SO_REUSEADDR precisely so it can rebind
  immediately, and Linux honours that. The flag is per socket and must be set before
  `bind()`, as on Linux. Every other option still answers success unchanged.
- Lazy reaping in `time_wait_snapshot`; a full 64-slot table **fails open** (the newest
  reservation is dropped), so the degraded behaviour under pressure is today's
  behaviour rather than a bind that cannot be satisfied.
- `TIME_WAIT` is a strict **leaf lock**: every caller snapshots it *before* taking
  `SOCK_TABLES`, and `handle_close` records only after both `SOCK_TABLES` and the
  stack lock are released. This follows the same discipline `BOUND_PATHS` has.

### Deliberately omitted

- **No lingering socket, no TCP TIME-WAIT protocol state.** The smoltcp socket is
  still removed at close, so nothing absorbs a late duplicate segment or re-ACKs a
  retransmitted FIN. Using smoltcp's own TIME-WAIT would mean keeping a socket (and
  its 16 KB of buffers) alive past `close()` plus a reaper in the net daemon — far
  more machinery than the divergence warrants. **What is modelled is the port
  reservation, not the protocol.**
- **No conflict check against live bound ports.** `bind()` has never had one: binding
  a port a live listener already holds still returns 0. Adding that is a strictly
  larger behaviour change with its own blast radius and is not what item 11 describes.
  The visible asymmetry — a dead-but-recent port is refused while a live one is not —
  is deliberate and commented in the source.
- No `SO_LINGER`, no `SO_REUSEPORT`, no `tcp_tw_reuse` / `tcp_fin_timeout` knobs.
- No TIME_WAIT visibility (no `/proc/net/tcp`, nothing in `getsockname`).
- No reservation for UDP. A connected UDP socket lands in the same `InetConnected`
  arm and is skipped by the `sock_type == SOCK_STREAM` guard. **That guard is
  load-bearing, not cosmetic**: `SocketSet::get::<tcp::Socket>` panics on a type
  mismatch.
- SO_REUSEADDR is not inherited by accepted sockets (Linux copies `sk_reuse`).
  Irrelevant: an accepted socket is never bound.
- Setsockopt still returns 0 for options it does not implement. Unlike getsockopt —
  where a bogus success makes the caller read an unwritten buffer, the zbus
  SO_PEERPIDFD trap — a silently-ignored setsockopt only loses a tuning knob, and
  turning those into ENOPROTOOPT is an unrelated change.

### Blast radius for item 11

Much smaller than item 10, and confined to AF_INET TCP. The behaviour change is:
after a TCP connection closes, its port is refused for 60 s to a caller that did not
set SO_REUSEADDR. Nothing in the tree binds an explicit AF_INET port — every
`raw_bind_in` at HEAD passes port 0, and `userland/ping` uses SOCK_RAW/ICMP, which
takes the `IcmpBound` arm and is untouched. The COSMIC session is AF_UNIX throughout.

The one thing to watch is ephemeral-port pressure: every actively-closed connection
reserves an ephemeral port for 60 s. The pool is 32768–60999 (28 232 ports) and the
reservation table is capped at 64 entries, so this cannot starve anything.

---

## 4. Tests

### `unix_listen_strict` (in `afunix_listen_strict.patch`)

Eight assertions:

| | shape | expected | at HEAD |
|---|---|---|---|
| a | unbound AF_UNIX STREAM `listen()` | -1 / EINVAL 22 | **0** |
| b | `socket; bind; listen` | 0 | 0 |
| c | repeat `listen` with a different backlog | 0 | 0 |
| d | `listen()` on a socketpair end | -1 / EINVAL 22 | **0** |
| e | `listen()` on a connector awaiting accept | -1 / EINVAL 22 | **0** |
| f | `listen()` on the accepted socket | -1 / EINVAL 22 | **0** |
| g | unbound AF_UNIX **DGRAM** `listen()` | -1 / EOPNOTSUPP 95 | **0** |
| h | the listener still accepts a second connection | 0 / fd ≥ 0 | 0 / fd ≥ 0 |

**Must-fail-unpatched set: (a), (d), (e), (f), (g)** — five assertions.
**(b), (c), (h) pass at HEAD too and are explicitly NOT counted as evidence the fix
is live.** They exist so that "make AF_UNIX `listen()` always fail" and "re-arm the
address on every `listen()`" cannot pass.

Falsifying mutations, named line by line:

- Replace the whole new AF_UNIX arm of `handle_listen` with the old bare `ok_reply()`
  — (a), (d), (e), (f), (g) all read `rc=0` where the test demands `rc=-1`.
- Delete just `match tbl.socks[slot].state { SockState::UnixListening { .. } =>
  ok_reply(), _ => err_reply(-22) }` and leave the type gate — (a), (d), (e), (f)
  flip; (g) still passes.
- Delete just `if ty != SOCK_STREAM as u8 && ty != SOCK_SEQPACKET as u8 { return
  err_reply(-EOPNOTSUPP); }` — (g) reports errno **22 instead of 95**, because an
  unbound DGRAM socket is `Unbound` and falls into the `_` arm.
- **Move that same `if` below the `match`** — identical result for (g): 22, not 95.
  This is what pins the *order* of Linux's gates, which is the subtle half of the
  semantics and the part a plausible-looking fix gets wrong.
- Change the `UnixListening` arm to `err_reply(-22)` ("tighten everything") — (b) and
  (c) flip to -1 and (h) fails to accept.
- Re-run any of bind's arming work on the repeat `listen()` — (h) breaks, mirroring
  assertion (c) of the existing `inet_listen_twice`.

### `tcp_time_wait` (in `tcp_time_wait.patch`)

Five assertions:

| | shape | expected | at HEAD |
|---|---|---|---|
| b | rebind the port of a just-closed accepted socket | -1 / EADDRINUSE 98 | **0** |
| c | same rebind with SO_REUSEADDR first | 0 | 0 |
| d | an unrelated `bind(127.0.0.1:0)` still gets a port ≠ the reserved one | 0 | 0 |
| e | rebind the port of a listener that never carried a connection | 0 | 0 |

(The setup — bind/listen/connect/accept/close in that order — is (a); it is a
precondition, not a scored assertion. `accept()` only succeeds once smoltcp reports
`Established`, which is what proves the connection being closed was a real one.)

**Must-fail-unpatched set: (b) only** — one assertion. Stated plainly because it is
the honest number: (c), (d) and (e) all pass at HEAD and are shape guards.

Falsifying mutations:

- Delete `if let Some(p) = park { time_wait_add(p); }` at the end of the
  `InetConnected` arm of `handle_close` — (b) reads 0 instead of -1/EADDRINUSE.
- Delete `if port != 0 && !reuseaddr && resv[..nresv].contains(&port) { return
  err_reply(-98); }` in `handle_bind` — same, (b) reads 0.
- Delete only the `!reuseaddr` term from that condition — (c) flips: the SO_REUSEADDR
  rebind becomes EADDRINUSE. This is the guard on the interaction that makes the
  feature safe rather than harmful.
- Return `handle_setsockopt` to a bare `ok_reply()` — (c) flips, because the flag
  reads false at bind time.
- Widen `active_close` to include `CloseWait | LastAck | Closed | Listen`, or move
  `time_wait_add` into the `InetListening` arm or the generic `_` arm of
  `handle_close` — (e) flips: the idle listener's port gets parked and the rebind is
  refused. This is what distinguishes "park the active closer of an established
  connection" from "park every port that was ever closed".
- Remove `if reserved.contains(&p) { continue; }` from `alloc_ephemeral_port` —
  (d)'s `other_port != port` becomes probabilistic (≈1/28232 chance of colliding)
  rather than guaranteed. It is **not** a reliable single-run detector, which is
  precisely why (d) is listed as a shape guard and not as evidence.

To actually observe the falsification, revert only the `servers/net/src/lib.rs` hunks
of a patch and keep its `userland/scmtest/src/main.rs` hunks — the fix and its test
ship together in one patch.

### Expected scmtest counts

`report()` prints exactly one `<name>: PASS|FAIL` line per subtest, so the count is
the number of such lines.

| tree | count |
|---|---|
| `a0f2c46` (HEAD) | **30 PASS / 0 FAIL** |
| + `afunix_listen_strict.patch` alone | **31 PASS / 0 FAIL** |
| + `tcp_time_wait.patch` alone | **31 PASS / 0 FAIL** |
| + both | **32 PASS / 0 FAIL** |

**Identical on aarch64 and x86_64.** Neither new subtest is `#[cfg]`-gated, neither is
behind an env check, and both are invoked unconditionally from `main()`. Every early
return in both functions goes through `report()`, so **neither test can produce a
missing line**: an absent `unix_listen_strict:` or `tcp_time_wait:` line means the
binary died before reaching it, never a silent pass. The only per-arch difference in
the new code is the `SYS_SETSOCKOPT` number (aarch64 208 / x86_64 54), taken from
`kernel/src/syscall.rs`'s `mod nr` at lines 336 and 553.

**No existing subtest is at risk from patch 2.** Every `raw_bind_in` at HEAD passes
port 0, so no existing test binds an explicit port, and `alloc_ephemeral_port` skips
reserved ports. `test_inet_loopback_tcp` and `test_inet_listen_twice` do now park
their ports on close (roughly four reservations between them), well inside the 64-slot
table and invisible to everything downstream.

---

## 5. Build results

RELEASE only. No debug build was produced at any point.

**Combined tree (both patches applied), `./scripts/build-all.sh`, exit 0.**

- aarch64: userland workspace + `scmtest` (54 208 bytes) + kernel + populated F2FS
  image, no errors.
- x86_64: userland workspace + `scmtest` (54 128 bytes) + kernel + populated F2FS
  image (`f2fs-data0-x86_64.img`, 2 042 626 048 bytes), no errors.
- No `error` or `error[...]` lines anywhere in the log. The only warnings are the
  pre-existing `non_upper_case_globals` ones in `drivers/src/drm_device_interface.rs`
  (`VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs`), which are untouched by this lane.
- `brush`, `coreutils` and `bottom` were skipped — those sibling repos are not
  reachable from a worktree path. That is a worktree artefact, not a build failure,
  and the images are otherwise complete.

**Each patch alone**, to back the "independently landable" claim:

| | `net-server` aarch64 | `net-server` x86_64 | `scmtest` aarch64 | `scmtest` x86_64 |
|---|---|---|---|---|
| `afunix_listen_strict.patch` | clean | clean | clean | clean |
| `tcp_time_wait.patch` | clean | clean | clean | clean |

(`cargo check -p net-server --release --target targets/<arch>-unknown-kernel.json`
with the same `-Z build-std` flags `build-all.sh` uses, and
`cargo build -p scmtest --release --target <arch>-unknown-none`.)

**Style gates.** `cargo fmt --check` and `clippy` are not gates in this repo (fmt
emits thousands of diff lines on pristine HEAD; clippy fails to compile at HEAD with
hard errors in `sched`). Both touched files rustfmt-**parse** cleanly
(`rustfmt --edition 2021 --emit stdout`, exit 0, no diagnostics), and the release
build emits no new diagnostics inside the added line ranges.

---

## 6. Recommendation

### Item 11 (`tcp_time_wait.patch`) — safe to land now

Confined to AF_INET TCP, which nothing in the tree and nothing in the COSMIC session
uses. Its worst realistic failure is that a hypothetical TCP server has to wait 60 s
to restart, which is the Linux behaviour it is implementing. It carries its own guard
test, and the only assertion that changes meaning against an unpatched kernel is
`tcp_time_wait` (b). It does not need a live desktop session — a plain `scmtest` run
on a fresh image on both arches is sufficient validation.

The one thing a reviewer should look at deliberately is the SO_REUSEADDR half. It is
not optional decoration: without it this patch would make server restarts *worse*
than the divergence it fixes, because `NET_SETSOCKOPT` was a bare `ok_reply()` and the
flag was being discarded.

### Item 10 (`afunix_listen_strict.patch`) — **do not land before a live COSMIC session**

The patch is correct and I would defend the semantics. It is also a no-op for every
healthy AF_UNIX server: the only state a working server is in at `listen()` time is
`UnixListening`, and that arm still answers 0, idempotently. On the code as written
there is no path by which a server that binds successfully can be affected.

That is exactly why I still recommend holding it. The argument for landing it blind
rests entirely on "no in-tree caller is affected", and the in-tree audit (scmtest and
wakepolltest, both of which bind first and check) is *not* the population at risk. The
population at risk is the out-of-tree session — cosmic-comp, cosmic-panel, busd, the
tokio/zbus stacks — and the specific hazard is not a healthy server but a component
whose `bind()` fails on a dirty image and which today limps on as a zombie listener.
This project has already been bitten by `/data` surviving reboots and by stale nodes
(`xattr_list_f2fs`, the `unlink_rebind` case); a stale `S_IFSOCK` under `/run/user/N`
is not a hypothetical. After this patch that component exits at `listen()` instead,
and `launch_pad` restarts it in a loop — which in a session log looks exactly like the
crash-loop signatures this project has spent whole waves chasing (M7v). Turning a
silent partial failure into a restart loop is a real risk even when the patch is
right, and it is undiagnosable from a build.

The cost of waiting is one `scmrun.py scmtest` plus one COSMIC boot on each arch. The
cost of being wrong is a wave spent bisecting a restart loop back to a listen()
tightening. Land item 11 now; hold item 10 until a lane owns QEMU and can do:

1. `scmtest` on a **fresh** image, both arches → expect 31/0 with this patch alone
   (32/0 with both).
2. A full COSMIC session boot on aarch64, then x86_64, checking the serial log for
   any new `listen` EINVAL and for `launch_pad` restart churn.
3. Because the dangerous case is specifically a *dirty* image, run the session a
   second time against the image the first run left behind. That is the run that
   would expose a component relying on the zombie-listener behaviour, and a
   fresh-image-only validation would miss it.

If step 3 is clean on both arches, this is a safe landing.
