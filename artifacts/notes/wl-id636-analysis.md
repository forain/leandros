# wl_display error 0 "Unknown id: 636" — panel↔comp desync analysis (M7v prep)

Host-only, read-only source analysis. Goal: narrow the hypothesis space so the
M7v on-target tree wave instruments the ONE decisive thing instead of fishing.

Sources read:
- `servers/net/src/lib.rs` (K1/K1-A/5c43227): UnixRing, PendingFdBatch,
  handle_sendmsg, handle_recvmsg, handle_send, handle_recv, handle_accept,
  unix_stream_end.
- `kernel/src/syscall.rs`: sys_sendmsg/sys_recvmsg + net_blocking_op wrapper
  (+ the uncommitted UXTRACE, currently gated off).
- panel EGL path: `.../cosmic-panel-bin/src/xdg_shell_wrapper/space/egl_surface.rs`
  (smithay EGL over `wayland_egl::WlEglSurface` → Mesa software GLES2).

---

## 1. What id 636 almost certainly is

The panel is a `wayland_egl::WlEglSurface` client driving a smithay `GlesRenderer`
on Mesa **software** GLES2 (softpipe/llvmpipe swrast). Mesa's wayland-EGL swrast
backend does NOT use dmabuf; it uses **`wl_shm`**: per color buffer it calls
`wl_shm.create_pool(fd, size)` (an **fd-carrying** request via SCM_RIGHTS), then
`wl_shm_pool.create_buffer`, plus `wl_display.sync` → `wl_callback` throbbers
around `eglSwapBuffers`. Mesa runs these on a **private `wl_event_queue`** via
`wl_proxy_create_wrapper`/`wl_proxy_set_queue`, but on the SAME wl_display
connection / SAME client id space as the panel's own layer-surface objects.

636 is a high, client-allocated id → created deep in init, after globals +
layer-surface + the whole EGL/GLES bring-up. The single most likely identity is a
**Mesa swrast `wl_shm_pool` (created by the fd-carrying `wl_shm.create_pool`)** or
its `wl_buffer`/`wl_callback` neighbour. Load-bearing consequence: **the request
that CREATES 636 very likely carries an fd**, so the suspect path is the
SCM_RIGHTS branch, not the plain-data branch.

## 2. Why the signature means "one whole message dropped on a boundary"

`wl_display@1 error(code=0 invalid_object, "Unknown id: 636")` is emitted BY comp:
the panel sent a request whose target-object word == 636, but comp never saw the
request that allocated 636 as a NEW_ID. Wayland is a pure byte stream and each
message is self-delimiting (header = object-id, opcode, 16-bit size). Two facts
constrain the fault:

- If random bytes were dropped/duplicated mid-message, comp's framing would
  derail into garbage opcodes / absurd sizes, NOT a clean plausible-looking id.
- A **clean** "unknown id 636" ⇒ framing stayed intact ⇒ an **integral number of
  complete messages vanished on message boundaries** (the byte after the gap is a
  valid next header). Losing exactly the create-636 request (a whole message,
  possibly fd-carrying) does exactly this.

So the search is specifically: *what makes a complete client→server message
disappear from `ring_ab` while leaving byte-framing aligned?* (panel = end A /
connector, comp = end B / acceptor ⇒ client→server is the **a→b** direction:
`ring_ab`, `fdq_ab`.)

## 3. Inspection result of the kernel socket path

The ring/fd machinery is **byte-exact by inspection** — this CONFIRMS the M7u
audit rather than overturning it:

- `UnixRing::write` returns exactly the bytes copied (`len.min(free)`); `read`
  returns exactly `len.min(count)`; `wtotal/rtotal` are monotonic and updated in
  lockstep. No silent truncation, no over-report.
- `handle_sendmsg` fd-path pins the batch to `seq = wtotal` (first byte), writes,
  breaks on partial, returns the true `total`; on `total==0` returns EAGAIN and
  **drops** the batch so libwayland (which only `close_fds` after a `!= -1`
  return) re-exports on retry. Partial-with-fd matches Linux + libwayland flush.
- `handle_recvmsg` one-batch-per-recv `max_read` cap: invariant
  `rstart ≤ fdq[0].seq_byte < fdq[1].seq_byte` holds across recvs (delivery pops
  only index 0 at end-of-recv when `seq_byte < rtotal`), so `max_read` is always
  `> 0`, never underflows, and never skips bytes — it only chunks. fd delivery
  order preserved; MSG_CTRUNC drops only overflow fds, never data.
- `handle_accept` preserves `conn_idx` + both rings + `wtotal/rtotal` + `fdq_*`;
  pre-accept fd/data sends survive the accept transition. No reset bug.

I could NOT construct a kernel path that makes a whole a→b message vanish while
keeping framing aligned. Two real Linux-divergences exist but neither drops
bytes (see H3). ⇒ raise the prior that the fault is **userspace (H4)**, but the
kernel cannot be cleared without on-target byte accounting because inspection
already missed nothing and the bug is load-dependent (only the heavy multi-fd
client trips it).

## 4. Ranked hypotheses

### H1 — Kernel drops/miscounts a→b bytes under the partial-write × SCM_RIGHTS interleave (LOW prob, HIGHEST impact)
The panel is the first client to combine all three at once: 2-iov **wrapped**
sends (libwayland's out-ring wraps → 2-entry iov), fd batches pinned to
`seq_byte`, and frequent **partial writes** as the 4096-byte `ring_ab` saturates
behind a busy compositor. Any one call that reports more (or fewer, re-sent)
bytes than it stored = a boundary drop. Inspection says exact; only live
byte-accounting can prove it.
- (a) Instrument: `handle_sendmsg` (BOTH branches) + `handle_send`
  UnixConnected/PendingAccept, a→b only; `handle_recvmsg` + `handle_recv`, a→b.
- (b) Invariant: `Σ total_written(a→b)` (kernel-accepted) must equal final
  `ring_ab.rtotal`; and at the instant comp raises the error, comp's cumulative
  recv on a→b must have PASSED the `wtotal` offset at which the create-636
  message was written. `sent_cum > recv_cum + ring_ab.count` at any sample = drop.
- (c) Predicted fix: whatever the gap localizes; the standing candidate is H3.

### H2 — `max_read` one-batch-per-recv cap mishandled at a boundary (LOW)
The `q_len >= 2` branch is exercised for the FIRST time by heavy fd traffic
(cosmic-bg's single pool never queues ≥2 batches). Not a byte-loss by
construction, but list it to rule out empirically.
- (a) Instrument: log whenever `max_read != usize::MAX` → `(rstart, second_seq,
  cap, nread, nfd_delivered)`.
- (b) Invariant: `cap > 0` always; `nread ≤ cap`; batch 0 delivered iff
  `fdq[0].seq_byte < rtotal_after`. Any `cap==0` or wrapped-huge `cap` = bug.
- (c) Predicted fix: none unless the trace shows a degenerate cap; if it does,
  guard `second <= rstart` explicitly.

### H3 — plain-path full-ring send returns 0 (not EAGAIN); asymmetry with fd-path (REAL latent bug; livelock, not drop)
`handle_send` UnixConnected/PendingAccept stream branch returns `val_reply(0)`
when `ring_ab` is full — unlike `handle_recv` (converts 0→EAGAIN) and unlike the
fd-path (returns EAGAIN on `total==0`). `net_blocking_op` only retries on -11, so
0 passes straight to libwayland, which treats it as "sent 0, retry" and
**busy-loops in wl_connection_flush** (no tail advance ⇒ no byte drop). This is
the likely mechanism behind the M4 "slow-vs-stuck under TCG": a burst of
fd-carrying pool/buffer messages fills the 4096 ring, then the *next* plain
request (which may be create-636's neighbour or a commit) spins at 0 instead of
parking on POLLOUT. It is NOT itself a drop, but it is a genuine bug worth fixing
and it can mask/perturb timing around the failure window.
- (a) Path: `servers/net/src/lib.rs` handle_send, the two AF_UNIX stream write
  branches (`UnixConnected` and `UnixPendingAccept`).
- (b) Invariant to confirm: count how often a→b `handle_send` returns 0 with
  `len>0 && !peer_closed` right before the error.
- (c) Fix: when `n==0 && len>0 && !peer_closed`, return `err_reply(-11)` (EAGAIN)
  so the blocking wrapper / libwayland poll-retry instead of spinning — mirror
  the handle_recv EOF-vs-EAGAIN logic on the send side.

### H4 — No kernel byte-loss; desync is Wayland/Mesa userspace (MEDIUM–HIGH if UCK shows sent==received)
If cumulative a→b sent == received (kernel delivered every byte), then 636 was
never created-and-flushed before use: a Mesa software-EGL private-queue / proxy
issue (`wl_proxy_create_wrapper` + `wl_proxy_set_queue`, or a `wl_display`
roundtrip on the EGL queue) where the NEW_ID request for 636 is marshalled but a
request TARGETING 636 reaches the wire/gets dispatched first — or an id
allocated on one queue and used before its create flushes. This pivots M7v OFF
the kernel entirely.
- (a) Instrument: at the sendmsg a→b boundary, dump the leading **8-byte wl
  header** (object-id u32, opcode u16, size u16) of each framed message.
- (b) Check: does a message with NEW_ID arg == 636 appear on the wire, and does
  it appear BEFORE any message whose target object-id == 636? If 636's creator is
  absent from the a→b byte stream (or ordered after a use of 636), it is a
  client/Mesa bug, not the socket.
- (c) Fix: userspace — pin Mesa's swrast objects to the right queue / force a
  flush+roundtrip after pool creation, or disable the private-queue wrapper path.
  (Out of scope for the kernel wave; hand back to the Wayland-userspace lane.)

## 5. TRACE THIS FIRST (one-liner for M7v)

> In `servers/net` for the panel's conn (a→b dir): log `[UCK-S wtotal_before,
> requested, total, nfd, hdr8]` in handle_sendmsg (both branches) + handle_send,
> and `[UCK-R rtotal_before, nread, cap, nfd, ctrunc]` in handle_recvmsg +
> handle_recv. One question decides everything: at the rtotal where comp raises
> "unknown id 636", did the panel's cumulative SENT bytes include the message
> whose 8-byte header carries new-id 636, and did comp's cumulative RECV reach
> it? **sent > recv gap ⇒ kernel drop (start at H3's send path); sent == recv ⇒
> userspace/Mesa (H4), stop touching the kernel.**

Practical: reuse the already-present `uxtrace` scaffold in
`kernel/src/syscall.rs` (flip `UXTRACE`), but the load-bearing counters
(`wtotal/rtotal/seq_byte/hdr8`) live in `servers/net` — which has no serial; add
a tiny ring-summary that syscall.rs prints, or thread the numbers back through
the Message reply (the recvmsg/sendmsg reply already carries a value word;
piggyback wtotal/rtotal in spare data bytes for the a→b conn only). Gate hard
(single `const`) so it's zero-cost when off.

---

### Checkpoint
- 2026-07-25: Analysis authored (Opus deep-reasoner, host read-only lane).
  Conclusion: kernel socket path is byte-exact by inspection (confirms M7u); no
  whole-message-drop path found. 636 ≈ Mesa swrast wl_shm_pool/buffer/callback,
  its creator likely fd-carrying (wl_shm.create_pool). Ranked H1(kernel drop,
  low/high-impact) > H4(userspace Mesa queue, med-high) > H3(real latent
  0-vs-EAGAIN send bug, livelock not drop) > H2(max_read, rule-out). Decider =
  cumulative a→b [UCK] byte accounting correlated to the error's rtotal + the
  8-byte header of the create-636 message. If sent==recv, pivot to Mesa
  private-queue/proxy analysis, not the socket. H3 fix (handle_send full-ring
  0→EAGAIN) is worth landing regardless.
