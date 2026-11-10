# lane/vifcore (podnet P1) - 2026-11-10 - DELIVERED

Pure refactor, no behaviour change: per-netns interface table (servers/net/src/vif.rs, vifsel.rs),
64 network stack slots, netns-gone hook (sched::ns::set_ns_gone_hook). Prerequisite for tundev/tapnet (pasta).

Merge with main (nlns, setns, netnsfix, credxlate, idscm): main's netns owner userns table (OWNER_UNS,
`sched::ns::owner_userns(kind, id)`) kept; vifcore's duplicate dropped, vif.rs calls `owner_userns(1, ns)`.
The death hook had no equivalent on main and stays.

Evidence: aarch64 build ok; Mac aarch64 nettest + nstest 0 fails, ffsandbox RESULT on=True off=False;
box x86_64 full regress PASS (736s); box aarch64 full regress PASS.
