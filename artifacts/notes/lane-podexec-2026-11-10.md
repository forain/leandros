# lane/podexec 2026-11-10 (podman wave, M6 mntlinks core)

FIXED: absolute/relative symlinks inside a pivoted mount-namespace root (`crun: executable file echo not found in $PATH`).

Root cause: for mntns tasks the VFS got the backing path and the filesystem resolved links against the backing volume root; an absolute link
(`/bin/echo -> /bin/busybox`) therefore named the host's /bin/busybox. `access()` (crun's PATH search) gave ENOENT; execve reads the backing
path directly and never went through the VFS link rewrite at all. `stat` only worked by luck of the legacy fallback.

Fix: `vfs::ns_resolve_links` walks the namespace path hop by hop (translate per prefix, generic backing readlink, absolute target re-anchors
at the ns root, `..` floors at the root, 40 hops). `rewrite_one` uses it for every `path_args` tag when a CALLCTX exists for the pointer and
registers a context for the rewritten buffer (cleared after dispatch: a stale entry was found again when the allocator reused the address).
`sys_execve_kp` and the shebang interpreter go through `ns_follow_links`.

Evidence: mnttest cases abs_link_in_root(+_disk), rel_dotdot_link(+_disk), fd_path_after_pivot: 5 FAIL on 09656944 (box), 0 fails after.
Podman: past exec lookup, now blocked at the conmon attach socket (lane attachsock).
Open: link reached through /proc/self/fd/N/<link>; xmount_link/procfd_target cases of M6 not done; cost is one readlink per path component for mntns tasks only.
