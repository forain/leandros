# lane/nstcp (S1 of design-podnet3) - 2026-11-10

Result: FIXED (the TCP paths themselves were already routed per netns; the bug was a netns-id reuse race).

- tuntest gains `tcp_out` (nonblocking connect over tap0, SYN frame read from the fd, injected SYN-ACK, data, FIN) and
  `tcp_in` (listener in the netns, injected SYN -> SYN-ACK, ACK, accept, data recv): 13 checks in a second netns.
- Fail-first: on unfixed main the new case failed 3 of 4 boots, each differently (connect ENETUNREACH, tap
  setup EOPNOTSUPP, no SYN on the fd), because tuntest's first netns child had just exited.
- Cause: `tunbe::attach` created/looked up `NETNS[ns]` without consuming the `GONE` bit, so a new netns that reused the id of a
  dead one inherited its stale state, and the daemon's later `reap(ns)` freed the NEW tap's stack. Only `vif::set_lo` did the claim.
- Fix: `vif::claim(ns)` (the GONE check + reap, factored out of set_lo), called first in `tunbe::attach`.
- After fix: tuntest 13/13 new + old green on two aarch64 boots.
- Open: TCP over rtnl-configured routes needs S2 (rtnlw); IPv6 TCP stays gated (-101).
