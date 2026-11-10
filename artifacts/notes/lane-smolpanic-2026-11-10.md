# lane/smolpanic 2026-11-10 - FIXED (panic); exit-127 is a SEPARATE open bug

Bug: net server panic "handle does not refer to a valid socket" (smoltcp socket_set.rs:116) during container churn.
Root cause: a veth DELLINK / netns reap frees a stack (STACKS[sid] = None, slot reusable) while a SockEntry
(InetConnected/IcmpBound {sid, handle}) or a udpany mirror still names its handle; the next op called
SocketSet::get_mut on it. Handles had no generation, so a reused stack/slot could also alias a different socket.

Fix: vendored smoltcp SocketSet handles carry set+slot generations; contains/try_get/try_get_mut/try_remove.
net lib.rs heal_stale() demotes a stale connected/ICMP entry to the unconnected state before every fd op
(error, never panic); release_inet_state skips dead handles; tcp_connect_status uses try_get_mut;
udpany::of() drops mirrors whose stack is gone. listen.rs cannot go stale: its queue/arm handles live in the
owning stack's own `listens` list and die with it.

Evidence: brtest stale_stack with heal disabled -> KERNEL PANIC (same message); with fix 9/9, no panic.
brtest 167/0 in a first run. churn127loop 40x aarch64 HVF: panics 0, exit-127 5/40 (iters 6,7,25,27,31):
the panic was NOT the 127 cause; 127 stays open (see lane shmlock/inotifyc127 notes).
Note: earlier churn attempts hung at the login prompt (driver.py login root root needed first).
Back-to-back `/bin/brtest /bin/brtest` in one boot: run 2 fails bridge_l2/fwd_* with errno 98 (EADDRINUSE,
fixed ports left from run 1); not the stale-stack bug (no errno 9 seen), brtest is not re-runnable in one boot.
Baseline: fwd_mac_steal precondition (errno 98) fails identically on main. nettag2test connect_pending/udpany_recv
failed under load in the full regress, pass 2/2 alone (known wake-count flake).
