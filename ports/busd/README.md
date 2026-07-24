# busd (LeandrOS port) — W1 investigation

`busd` (crates.io **0.5.0**) is the D-Bus broker for the COSMIC session bus.
`build.sh` builds it static-musl for both arches (see that file for the
static-PIE landmine and the mandatory `-C relocation-model=static`).

## Status: W1 is a KERNEL async-runtime defect, NOT a busd userspace bug

The "cosmic-comp deadlocks talking to busd" wall (W1) was believed (M6g) to be a
busd/zbus userspace async bug to be patched here. **M6h disproved that.** The
busd-level fix in `current-thread-runtime.patch` (and three further attempts) do
**not** fix W1. Root cause and evidence below; the actual fix belongs in the
kernel poll/wake path and is tracked as an M7 item.

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
- A 50 ms `tokio::time::interval` keepalive — **busy-spins**: the tokio *time
  driver* also misbehaves (durations not honored), starving the accept loop.

Both the default multi-thread runtime and current_thread fail identically, so
this is not a runtime-flavor issue. It is the same class as the M4 "client
roundtrip stalls under TCG" and the kernel's `POLL_SAFETY_WAKE=false` note
(a pure lost wake hangs).

## M7 next step (the real fix)

Fix the kernel so that a wake posted against a task blocked in
`epoll_wait(infinite)` (tokio's reactor waker-eventfd, `sched::wake_poll`) — or a
freshly-scheduled task — reliably interrupts the park. Then rebuild busd with
`build.sh` (the current_thread patch is a reasonable belt-and-suspenders but is
neither necessary nor sufficient on its own) and re-run the W1 validation
(cosmic-comp → busd → Hello replied → comp reaches serving with the bus).

## Build

`./build.sh` — needs `cargo +nightly` with the musl targets. Produces ET_EXEC
static binaries staged to
`~/code/leandros-artifacts/m5-session-ship/<arch>/usr/libexec/busd`.
