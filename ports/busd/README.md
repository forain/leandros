# busd (LeandrOS port) — W1 investigation

`busd` (crates.io **0.5.0**) is the D-Bus broker for the COSMIC session bus.
`build.sh` builds it static-musl for both arches (see that file for the
static-PIE landmine and the mandatory `-C relocation-model=static`).

## Status (M7e, 2026-07-24): W1 is a USERSPACE tokio-runtime WEDGE — kernel EXONERATED

**Corrected verdict.** M6h's "W1 is a kernel poll/wake defect" framing is now
DISPROVEN, and so is M7c's "zbus internal-executor / async_executor" theory
(with the `tokio` feature, zbus's `async_executor` is `#[cfg(not(feature =
"tokio"))]` — not compiled; `Executor` is a zero-sized `PhantomData`,
`Executor::spawn` → `tokio::task::spawn`, and the `internal_executor` thread's
`while !is_empty` loop body never runs because `is_empty()` ≡ `true`). busd is a
**single `current_thread` tokio runtime**; the per-peer `socket_reader` is a
plain `tokio::spawn` from the accept-loop task.

The kernel wake machinery is now comprehensively proven sound (wakepolltest
38/0, including the new **same-thread EPOLLET/level eventfd re-arm** coverage —
the exact busd/mio reactor primitive, previously untested; plus cross-thread
eventfd/pipe/AF_UNIX, timed-futex cross-thread `FUTEX_WAKE`, timerfd
deadline/periodic). M7a (0a1a9b7 poll-deadline) and M7b (db0cfdb timed-futex)
closed the only real kernel lost-wake classes.

What W1 actually is (M7e, empirically): the moment busd finishes comp's handshake
and spawns comp's `socket_reader` (`busd::peer: created`), busd's **entire tokio
runtime freezes** — while the rest of the system (the shell) stays responsive.
Proven across FOUR configs, all identical freeze at `peer: created`:
`current_thread`; `current_thread` + a 100 ms in-runtime `tokio::time::interval`
keepalive (the interval fires in perfect cadence until the connect, then stops);
`current_thread` + a foreign-thread `handle.spawn` pacemaker (a *cross-thread*
unpark — the wake class wakepolltest proves reliable — which also stops firing at
the connect); and **stock `multi_thread`**. Because a cross-thread unpark (kernel-
proven) does NOT rescue it, the runtime is **not in a recoverable park** — it is
wedged (busy-loop or self-deadlock) in the tokio/zbus scheduling path. No
"wake it up" workaround can land W1.

### Next step (escalated): userspace, needs a Linux (Alpine) diff env

The fix is a vendored tokio/zbus/busd patch, not a kernel change. Decisive next
move: run **this exact busd 0.5.0 + zbus 5.13.1 + cosmic-comp** on Alpine
(x86_64/aarch64 docker) to determine whether the wedge is LeandrOS-specific or a
busd/zbus version bug that also hangs on Linux; and build the minimal
`current_thread` repro (accept-loop root future → multi-round handshake on the
peer fd → `tokio::spawn` a reader over a socket with **pre-buffered coalesced
data** → re-park) traced with the M7b kernel ring-tracer to establish
parked-vs-deadlocked and local-vs-inject at the wedge. The `current_thread`
patch below is retained (it is the current shipped flavor) but is neither
necessary nor sufficient; multi_thread fails identically.

## What actually happens (byte/marker-exact, M6h)

1. cosmic-comp connects; busd accepts and completes the auth handshake
   (AUTH EXTERNAL → OK → NEGOTIATE_UNIX_FD → AGREE → BEGIN → "Handshake done" →
   `busd::peer: created`). The kernel delivers every byte (M6g proved this).
2. Right after the handshake, busd (via zbus) spawns the per-peer `socket_reader`
   tokio task, which holds comp's already-buffered `Hello` and would parse+reply
   it on its **first poll** (128 B ≥ MIN_MESSAGE_SIZE → zero further syscalls).
3. **That task is never polled.** busd's reactor sits in `epoll_wait(timeout=
   INFINITE)` (kernel syscall trace: busd's last call is `epoll_wait fd=.. v=
   0xFFFF…FFFF`, then silence) and is never woken to drain the ready task. comp
   blocks forever awaiting its unique-name reply.

The distinguishing fact: tokio tasks spawned **before** the accept loop starts
(busd's self-dial connection's socket_readers) DO run; a task spawned **while the
reactor is parked in `epoll_wait(infinite)`** (every real client's socket_reader)
does NOT. The internal wake that should interrupt the infinite park to run a
newly-ready task — tokio's waker-eventfd write → epoll edge, or the current_thread
scheduler draining its run queue before the infinite park — is not delivered on
LeandrOS/QEMU.

### Attempts that did NOT fix it (all reproduced the identical stall)

- `#[tokio::main(flavor = "current_thread")]` (`current-thread-runtime.patch`) —
  removes the multi-thread inter-worker unpark, still stalls.
- Inlining `Peers::add()` into the accept loop (top-level instead of nested
  spawn) — still stalls.
- `tokio::task::yield_now()` after the peer is added — **never returns** (the
  runtime never resumes the yielded task).
- A 50 ms `tokio::time::interval` keepalive — busy-spun at the time (the tokio
  *time driver* misbehaved). **M7e update: no longer true** — after the M7a
  poll-deadline (0a1a9b7) and M7b timed-futex (db0cfdb) kernel fixes, a 100 ms
  interval fires in perfect cadence (verified). It still does not fix W1: the
  interval keeps ticking cleanly *until* comp connects, then stops together with
  the whole runtime at `busd::peer: created` (the runtime freezes, not just the
  reader).

**M7e correction to items 2-3 above:** the reactor is NOT merely "parked in
epoll_wait(INFINITE) with a lost wake." It is WEDGED — a foreign-thread
cross-thread unpark (kernel-proven reliable) does not rescue it either — so it is
a busy-loop / self-deadlock, not a recoverable park. And it is not runtime-flavor
specific: stock `multi_thread` freezes identically at `peer: created`.

## M7e next step (escalated — userspace, needs a Linux diff env)

The kernel is exonerated (wakepolltest 38/0 incl. the same-thread eventfd re-arm
primitive; see the Status section). The remaining fix is a vendored
tokio/zbus/busd patch. Decisive next moves: (1) run this exact busd 0.5.0 + zbus
5.13.1 + cosmic-comp on **Alpine** to test LeandrOS-specific vs. a busd/zbus
version bug that hangs on Linux too; (2) build the minimal `current_thread` repro
(accept-loop root → multi-round handshake on the peer fd → `tokio::spawn` a reader
over a socket with pre-buffered coalesced data → re-park), trace it with the M7b
kernel ring-tracer to pin parked-vs-deadlocked and local-vs-inject at the wedge.

## Build

`./build.sh` — needs `cargo +nightly` with the musl targets. Produces ET_EXEC
static binaries staged to
`~/code/leandros-artifacts/m5-session-ship/<arch>/usr/libexec/busd`.
