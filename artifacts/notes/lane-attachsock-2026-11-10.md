# lane/attachsock 2026-11-10 (podtrace5 blocker B: conmon attach socket)

FIXED. Root cause is NOT the /proc/self/fd/N sun_path (bind/listen/connect through it worked): a SOCK_SEQPACKET connector that
send()s BEFORE the listener accept()s (podman dials and writes at once; conmon accepts later) hit the pre-accept
`UnixPendingAccept` branches of servers/net handle_send/handle_recv, which were stream-only: the send wrote raw bytes into ring_ab
with no 4-byte record header, so the accepted end's read_dgram took "ping" as the record length and failed EFAULT (short ring read).
Recv on a pre-accept connector read ring_ba as a byte stream the same way.
Fix: for sock_type != SOCK_STREAM those two branches use write_dgram / read_dgram|peek_dgram (framed records, MSG_TRUNC, EAGAIN, wakes).
Tests (scmtest): seqpacket_long_dir_via_procfd (>108-byte dir, bind/connect via /proc/self/fd/N/attach, forked connector),
seqpacket_short_path_forked, seqpacket_short_path (control). Fail-first (aarch64 HVF): long_dir and short_forked FAIL with recv ret=-1 errno=14; after fix all PASS.
