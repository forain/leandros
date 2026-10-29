# lane/rtnetlink2 (2026-10-29) - FIXED

servers/net/src/rtnetlink.rs: dump only when flags & NLM_F_DUMP != 0; non-dump
RTM_GETROUTE answered with a single NEWROUTE; GETNEIGH dump answered with DONE.
netlinktest +4 cases. Design: meetdiag2-2026-10-29.

Evidence: host unit tests 7/7; netlinktest 40/0 on aarch64 and 0 fails on
x86_64; regress PASS on aarch64; nettest 0 fails on x86_64.
