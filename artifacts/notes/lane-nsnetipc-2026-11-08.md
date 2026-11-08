# lane/nsnetipc 2026-11-08 (L4 nsipc + L5 nsnet)

FIXED. Net and IPC namespace ids live in sched/src/ns.rs (new_flat, flat/parentless ids), Task.netns/ipcns
(refcounted, inherited by both clone paths). CLONE_NEWNET/NEWIPC accepted by clone/clone3/unshare when
ns_capable(CAP_SYS_ADMIN) (implicit with NEWUSER); not with CLONE_THREAD/VM; unshare needs single thread.
Net server: abstract AF_UNIX names scoped by the calling task's netns at bind/connect; AF_INET/INET6 in
non-initial netns: connect/sendto -> ENETUNREACH, bind to a specific addr -> EADDRNOTAVAIL, wildcard bind ok.
Path sockets unscoped. Before: clone(NEWNET|NEWIPC) was EINVAL (old nstest asserted that).
Tests: nstest netns_ipcns, netns_isolate_reverse PASS (aarch64); nettest PASS.
Open / limits: netns taken from the calling task, not captured per socket at creation (fd passing / unshare after
socket() uses the caller's ns); wildcard INET binds share the host port space (no per-ns stack); no
/proc/<pid>/ns/{net,ipc} links (TODO: report netns_of/ipcns_of ids via nsproc's hook once merged).
