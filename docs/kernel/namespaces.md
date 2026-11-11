# Namespaces, capabilities and sandboxing

LeandrOS implements the Linux namespace and sandboxing surface that Firefox's content sandbox and rootless `podman` need: user, PID, mount, network, UTS, IPC and cgroup namespaces, per-thread capabilities, seccomp-BPF, `setns(2)` and nsfs file descriptors, and a cgroup v2 filesystem. All of it lives in `sched` (identity, tables, checks) with the path-dependent parts in `servers/vfs` and `servers/net`. This page describes the model; container-level behaviour is in [userland/containers.md](../userland/containers.md).

## Overview

| Piece | Summary |
|---|---|
| Namespace table | [`sched/src/ns.rs`](../../sched/src/ns.rs): one lock-free table of `NS_MAX` = 256 slots shared by every namespace kind; id 0 of each kind is the initial namespace and is never counted. A slot has a refcount, parent, level, owner uid/gid and a per-slot kind. |
| `NsRef` | A counted reference. `Task` holds one per namespace kind; fork and thread clone take their own, task reap drops it, and the parent chain is released when a slot frees. |
| Capabilities | [`sched/src/caps.rs`](../../sched/src/caps.rs): per-thread `[effective, permitted, inheritable, bounding]`, plus ambient, securebits and subreaper (`prctl`). |
| User namespaces | `CLONE_NEWUSER` / `unshare`, composed uid/gid maps, quota. |
| PID namespaces | [`sched/src/pidns.rs`](../../sched/src/pidns.rs). |
| Mount namespaces | [`servers/vfs/src/mnt.rs`](../../servers/vfs/src/mnt.rs): a per-namespace mount tree. |
| Network namespaces | per-netns interface table and stacks in `servers/net`; see [networking.md](../servers/networking.md#network-namespaces). |
| seccomp | [`sched/src/seccomp.rs`](../../sched/src/seccomp.rs) storage, [`lib/seccomp-bpf`](../../lib/seccomp-bpf) checker and interpreter. |
| cgroups | [`sched/src/cgroup.rs`](../../sched/src/cgroup.rs) core, [`servers/vfs/src/cgroupfs.rs`](../../servers/vfs/src/cgroupfs.rs) filesystem, [`sched/src/bpfcg.rs`](../../sched/src/bpfcg.rs) device programs. |

A fixed table is the same design choice as everywhere else in the servers: a namespace id is a small integer that can be stored in a task, a socket or a mount without allocation, and a leak shows up as a full table (`ENOSPC`) in a test instead of as unbounded memory.

## Capabilities and "host root"

Credentials in the kernel are host (kernel) ids; namespaces only change how ids are *shown* (see [id maps](#id-maps)). Two questions are therefore kept apart:

- `ns_capable(cap)` asks "does this thread hold `cap` in its own user namespace". The new-mount, chroot, `CLONE_NEW*` and `setns` gates use it, evaluated against the namespace that **owns** the object (the mount namespace's owner, the netns's owner), as Linux's `cap_capable` walk does.
- `host_capable(cap)` asks "is this euid 0 in the *initial* user namespace". Host-wide effects (loading kernel modules, `bpf(2)` program load and attach, `settimeofday`, device nodes) use it, so root inside a container cannot reach them. `crun` sees the resulting `EPERM` on `bpf` and treats the container as rootless.

Capability transitions follow Linux: `capget`/`capset` subset rules, `PR_CAPBSET_READ/DROP`, `cap_emulate_setxuid` on the setuid family, `execve` recomputing sets (set-id bits and file capabilities are ignored when the task's user namespace is not the initial one). `/proc/<pid>/status` prints `CapInh/CapPrm/CapEff/CapBnd/CapAmb`, `NoNewPrivs` and `Seccomp`/`Seccomp_filters`.

## User namespaces

- `clone(CLONE_NEWUSER)` and `unshare(CLONE_NEWUSER)`. `NEWUSER` with `CLONE_THREAD`, `CLONE_FS` or `CLONE_VM` is `EINVAL`; `unshare` needs a single-threaded caller; nesting deeper than 32 is `EUSERS`; a full table is `ENOSPC`. The child starts with a full capability set *inside* the new namespace only.
- **Quota.** A new namespace is charged against the creator's kuid **and the owner kuid of every ancestor user namespace** (Linux `inc_ucount`), each checked against 32 per user and 64 in total. Counts are derived by scanning live slots, so release paths need no decrement and cannot drift. Without the hierarchy, a user could exhaust the 256-slot table by nesting.
- **`CLONE_NEWUSER` for sandboxes.** An unprivileged process may create a user namespace and then, inside it, further namespaces: `CLONE_NEWNS`, `CLONE_NEWNET`, `CLONE_NEWIPC` and the rest are allowed without `CAP_SYS_ADMIN` when combined with `CLONE_NEWUSER` (the check is made in the new namespace).

### Id maps

`/proc/<pid>/uid_map` and `gid_map` hold composed maps (up to `MAP_EXT` extents per namespace). The write rules mirror Linux's `map_write`: one write, a parent-side mapping that must exist in the parent's view, and `CAP_SETUID`/`CAP_SETGID` in the parent for anything more than your own single id (which is how `newuidmap`/`newgidmap` get to write `/etc/subuid` ranges; the stock `leandro:100000:65536` is created for the login user). Mapping kuid 0 is refused.

**Non-identity maps are the default.** A container with `0 1000 1; 1 100000 65536` really runs its root as host uid 1000 and its file owners show as translated. The kernel cmdline knob **`idmap=off`** restores the old identity-only behaviour (write rule 9 then refuses `upper != lower`); it is an opt-out kept for one release. Translation happens at the user-memory boundary, never inside kernel state:

- `stat` owners and `getuid`-family results are translated on copy to user memory; `set*id`, `chown`, `setgroups` translate on the way in and fail `EINVAL`/`EPERM` for unmapped ids.
- Unmapped ids read back as 65534 (`overflowuid`) and creating a file whose owner would be unmapped is `EOVERFLOW`.
- File permission checks use capability *over the inode* (`CAP_DAC_OVERRIDE`, `CAP_FOWNER`, `CAP_CHOWN` held in a namespace in which the inode's owner is mapped), including f2fs; POSIX ACL named entries are translated through the caller's namespace; `trusted.*` and `security.*` xattrs are host-only.
- `SCM_CREDENTIALS` and `SO_PASSCRED` on AF_UNIX are implemented (they did not exist before) and translated at `recvmsg` into the receiver's pid namespace (an invisible sender reads pid 0) and user namespace (unmapped reads 65534). `SO_PEERCRED` is translated the same way. Senders may only claim their own ids unless they hold `CAP_SYS_ADMIN`/`CAP_SETUID`/`CAP_SETGID` in their user namespace (`scm_check_creds`).
- `signal`, `kill`, `ptrace`-class checks, `prlimit`, `setpriority`, SysV IPC and `waitid` compare credentials in the *target's* user namespace; `devpts` honours `uid=`/`gid=` mount options translated through the mounter's namespace (`EINVAL` if unmapped).

### ptrace and `/proc` access gate

One shared `ptrace_gate` (in [`sched/src/cred.rs`](../../sched/src/cred.rs)) answers Linux's `__ptrace_may_access` for `/proc/<pid>/{environ,ns/*}` and friends: ids must match **and the target must be dumpable**, unless the caller has `CAP_SYS_PTRACE` over the target's `mm->user_ns`. Tasks therefore track `mm_userns`; a process that dropped privileges with `setuid(65534)` is non-dumpable and its `mm_userns` stays the initial namespace, so a same-uid sibling cannot open its namespace files (rootless `pasta` depends on exactly this Linux behaviour). Do not "fix" such a denial by setting `PR_SET_DUMPABLE`.

## PID namespaces

`CLONE_NEWPID` makes the child pid 1 of a new namespace. Pids are translated through `pidns::from_cur`/`to_cur` at every syscall and `/proc` boundary: an invisible pid is `ESRCH`/`ENOENT`, `/proc/<n>/exe|cwd|root` magic links of a pid outside the namespace are not followable, and `NSpid:` is reported.

- **Init signal semantics (Linux `sig_task_ignored`).** A pid namespace's init with `SIG_DFL` for a signal drops it, except `SIGKILL`/`SIGSTOP` sent from an *ancestor* namespace. A signal that was **blocked** when sent is not ignored at send time (a blocked signal is never ignored): it is queued, and the delivery loop applies the same filter at dequeue time, so unblocking cannot let a queued `SIGTERM` kill init. This is what keeps `pasta` and `crun` helper "init"s alive while they wait in `sigwaitinfo`.
- **Teardown (`zap`).** When init dies, every task in the namespace is killed, found by `(tid, tgid)` so leaderless thread groups are included; the `SIGKILL` is re-sent on every loop pass (fork races), and `clone` re-checks `pid_dead` under the run-queue lock immediately before enqueueing. If the 5 s bounded wait times out, `[pidns] zap timeout: ns <id> init <tgid> still has <n> live task(s), first: <pids>` is printed (at most once per 10 s) through a `register_log_hook` callback, because `sched` has no logging of its own.
- Orphans reparent to the nearest live reaper (`reaper_for` skips dead namespaces and zombie inits); `PR_SET_CHILD_SUBREAPER` is honoured.

## Mount namespaces

Namespace 0 keeps the original alias table (`alias.rs` is now a shim over `mnt::ns0`) and every path in it resolves exactly as before. The first `CLONE_NEWNS` creates a real tree.

- **Data model.** `MntNs` is a tree of `Mnt` entries keyed by `NsId`: overmount stacks (the top of stack wins), file binds, propagation state (private/shared/slave/unbindable and peer groups), and `MNT_LOCKED` flags for mounts inherited into a less-privileged user namespace. Superblocks are a separate registry; a namespace copy shares them by reference, so a tmpfs created in one namespace is hidden from `/proc` listings of others and lives under the hidden prefix `/.leandros-sb` (never reachable by name; `resolve_ns` answers `ENOENT`).
- **Resolution.** `KPath` carries the mount id and namespace prefix length; the single path resolver (see [syscalls.md](syscalls.md#one-path-resolver)) dispatches to the namespace-aware walk **only** when `current_mntns() != 0`, so ordinary processes pay nothing. Symlinks are resolved in namespace space, so `exec` through links in a pivoted root works; `#!` and `PT_INTERP` resolve in the namespace too.
- **Syscalls.** `mount` handles bind/rbind (files and directories), tmpfs, proc/sysfs/devpts/mqueue/cgroup2 and mounts onto `/proc/self/fd/N`; `MS_REMOUNT`, `MS_MOVE`, propagation flags (`MS_SHARED/PRIVATE/SLAVE/UNBINDABLE`, `MS_REC`); `umount2` (plain, `MNT_FORCE`, `MNT_DETACH`, `UMOUNT_NOFOLLOW`; busy submounts are `EBUSY`); `pivot_root` (below); and the new mount API calls **`open_tree`** (including `OPEN_TREE_CLONE`), **`move_mount`** and **`mount_setattr`**. A detached tree is an anonymous mount namespace held by an nsfs-kind fd. `fsopen/fsconfig/fsmount/fspick` stay quiet `ENOSYS` so `crun` and `libmount` fall back to `mount(2)`.
- **Privilege.** Every mount operation needs `CAP_SYS_ADMIN` over the **owner user namespace of the mount namespace** (`may_mount`). Clones keep `MNT_LOCKED` children; a non-recursive clone that would drop locked children, and any attempt to clear a locked flag, fail (`EINVAL`/`EPERM`). `nosuid` is honoured in non-initial namespaces too (a bind of a setuid binary stays inert).
- **`pivot_root`.** `sys_pivot_root_ns` re-parents the old root under `put_old`, makes `new_root` the root, moves `MNT_LOCKED` from old to new, and `sched::rebase_fs_paths` rewrites the string root and cwd of every task sharing that fs state. The Linux checks are mirrored (`new_root` must be a mount point, `put_old` inside it, no shared parent, not locked, refused for a chrooted task). `umount("/")` is accepted for the root overmount stack, so the `crun` sequence `chdir(root); pivot_root(".", "."); umount2(".", MNT_DETACH)` works.
- **Per-mount flags are enforced.** `MS_RDONLY` gives `EROFS` for opens for write, `O_TRUNC`, creating `O_CREAT`, `mkdir/mknod/unlink/rmdir/rename/link/symlink/chmod/chown/utimes` and xattr writes (`mnt_flag_check`, called once from `rewrite_one`); `nodev` makes opening a device node `EACCES`; `noexec` makes `exec` `EACCES`. Descriptors already open for write keep working after a remount read-only, as on Linux. Flags are checked only for tasks outside namespace 0 (namespace-0 mounts have no per-mount flags). Not covered: `fchmod/fchown/futimens` on an fd, and fd-relative `*at` paths without a namespace context.
- **Introspection.** `/proc/self/mountinfo` prints mount ids, parent ids, propagation fields (`shared:N`, `master:N`) and per-mount options; `/` starts out `shared:1`. `/proc/mntselftest` exposes `ns_live/ns_dying/sb_count` for leak checks (dead namespaces are reaped when a new one is created).
- **devpts / tmpfs instances.** `devpts` mounts are refcounted superblocks with monotonic ids (`newinstance`, `ptmxmode`), and namespace copies of tmpfs aliases hold a reference to their instance, so an instance outlives the namespace that created it only while someone holds it.

## nsfs, `setns` and namespace files

`/proc/<pid>/ns/{user,pid,mnt,net,uts,ipc,cgroup}` (also under `task/<tid>/ns` and `thread-self/ns`) open as nsfs descriptors and can be `setns`'d, bind-mounted onto a file (how `podman` and `ip netns` pin a netns), and queried with `NS_GET_USERNS`, `NS_GET_PARENT`, `NS_GET_NSTYPE`, `NS_GET_OWNER_UID`. Opening another process's ns file needs the ptrace gate above. `setns(CLONE_NEWNS)` joins a mount namespace by nsfd or pidfd. Binding an ns file keeps the namespace alive (the bind holds a reference; closing the fd drops it) and `statfs` reports `NSFS_MAGIC`.

Internally an ns file is a typed path `/.leandros-ns/<kind>/<id>`. It is **not openable by name**: `resolve_ns` answers `ENOENT` for the prefix, and only the mount translation of a bound nsfs file reaches the VFS's synthetic branch. `hold_checked(kind, id)` rejects out-of-range ids, free or dying slots, and slots of another class before taking a reference, and `put` marks a slot dying by compare-and-swap before clearing parent/owner/level so a recycled slot cannot be stolen through a stale id. (Net, IPC and UTS share one slot class.)

## UTS, IPC and cgroup namespaces

- **UTS** (`CLONE_NEWUTS`): per-namespace `hostname`/`domainname`; `sethostname` needs `CAP_SYS_ADMIN` over the namespace's owner.
- **IPC** (`CLONE_NEWIPC`): SysV IPC and POSIX message queues (`mq_*`, `/dev/mqueue`, `mqueue` sysctls) are per namespace; Firefox's socket and content processes unshare it.
- **cgroup namespaces** (`CLONE_NEWCGROUP`): a cgroup root; paths are shown relative to it (with `/..` for outside).

### cgroup v2

[`cgroup.rs`](../../sched/src/cgroup.rs) is a 1024-node tree with atomics for hot state (parent, depth, hierarchical task count, `pids.max/peak/events`, `max.depth/descendants`, dead CPU time) and one mutex for names and owners (lock order: cgroup tree, then run queue). Fork admission (`fork_check`) charges `pids.max` and fails `EAGAIN`, counting `pids.events max`; zombies count until reaped. `migrate_tgid` enforces the no-internal-process rule (`EBUSY` when the destination has `subtree_control`), the cgns scope (`ENOENT` outside) and `pids.max`. The filesystem is mounted at boot on `/sys/fs/cgroup`, with controllers enabled and child `cpu`, `io`, `memory` and `pids` files; `cgroup.kill` and delegated `cgroup.freeze` work, and `/sys/kernel/cgroup/delegate` lists the delegatable files. Login sessions get a `user-<uid>.slice` that is delegated to the user, which is where `crun` creates a sibling cgroup for each container.

**Device controller.** `bpf(2)` accepts exactly `BPF_PROG_TYPE_CGROUP_DEVICE` programs (`PROG_LOAD`, `ATTACH`/`DETACH`/`QUERY`, `GET_FD_BY_ID`; `CAP_BPF|CAP_SYS_ADMIN`, `CAP_NET_ADMIN` or `CAP_SYS_ADMIN` in the **initial** user namespace). The in-kernel verifier and interpreter live in [`lib/ebpf-cgdev`](../../lib/ebpf-cgdev). Attached programs are enforced on device `open` and `mknod`, with the effective list computed hierarchically as Linux does.

## seccomp

`seccomp(2)` and `prctl(PR_SET_SECCOMP)` accept classic-BPF filters (`SECCOMP_SET_MODE_FILTER`, `SECCOMP_FILTER_FLAG_TSYNC`; `NoNewPrivs` or `CAP_SYS_ADMIN` required). A thread's filter is a refcounted chain, newest first, stored inline in `Task` so the syscall fast path tests it with one acquire load and no lock. The invariant that makes the lock-free read safe: a thread's head is only replaced by a node whose chain *contains* the old head (self-install pushes on top; `TSYNC` requires ancestry), so the old node survives through `prev` while the owning thread is still walking it. Writers hold `RUN_QUEUE`.

Evaluation runs with interrupts masked (the kernel is not preemptible), which with `MAX_INSNS_PER_PATH` = 32768 would be a long latency window. `evaluate` therefore uses `run_ticked`/`run_stack_ticked`, which call an interrupt-window closure every `TICK_INSNS` = 256 instructions inside a filter and between filters. A 4096-instruction filter run by four threads while another `TSYNC`s shows a worst 1 ms-loop gap of 4 ms.

## chroot and the void root

`chroot(2)` needs `CAP_SYS_CHROOT` in the caller's user namespace. A chroot to anything under `/proc` (the idiom Firefox uses with `/proc/self/fdinfo`) maps to `/.leandros-void`, an empty 0555 directory: lookups below it fall to the root mount and fail, `..` clamps, and already-open descriptors keep working. The void name itself is hidden (`ENOENT`) unless the caller's root already is the void.

## Firefox's content sandbox

Firefox's sandboxes (content, GMP, RDD, socket, utility) are **on by default** (`LEANDROS_FIREFOX_SANDBOX=0` in `firefox.sh` turns them off). Each child is created with user, net and (for content/utility/socket) IPC namespaces, the void chroot and a seccomp filter; the parent and fork server are unsandboxed by design. The acceptance harness [`ffsandbox.py`](../../.claude/skills/run-leandros/ffsandbox.py) prints per-process `Seccomp`, `NoNewPrivs`, root and namespace differences and exits non-zero unless every child shows `Seccomp: 2` with no sandbox error lines. Features added for it: `/proc/<pid>/fdinfo`, `SECCOMP_*` status lines, thread-group `TSYNC`, pidns magic-link translation, and `/etc/ld.so.conf` and `/etc/localtime` on disk (the sandbox policy reads them).

## Limitations and open work

- Net, IPC and UTS share one slot class, so `hold_checked` cannot tell them apart.
- `NPROC` and inotify limits are charged per kuid, not up the user-namespace tree.
- `fsopen/fsconfig/fsmount/fspick` are `ENOSYS`; overlayfs is not mountable (only the real-layer helper and pseudo-pid credential layer in `servers/vfs/src/ovl` exist), so containers use the `vfs` storage driver.
- `keyctl` is `ENOSYS`; raising a hard `rlimit` is still euid-0 based.
- `CLONE_NEWTIME` is unsupported.

## Code and tests

| Area | Source | Suites |
|---|---|---|
| Namespaces, ids, quota | [`sched/src/ns.rs`](../../sched/src/ns.rs), [`caps.rs`](../../sched/src/caps.rs), [`cred.rs`](../../sched/src/cred.rs) | `nstest`, `idmaptest`, `captest`, `chroottest` |
| PID namespaces | [`sched/src/pidns.rs`](../../sched/src/pidns.rs) | `nstest`, `forktest`, `sigtest` |
| Mount namespaces | [`servers/vfs/src/mnt.rs`](../../servers/vfs/src/mnt.rs), `sys_pivot_root_ns` in [`syscall.rs`](../../kernel/src/syscall.rs) | `mnttest` (incl. `pivot_suite`, `opentree_suite`), `/proc/mntselftest` |
| seccomp | [`sched/src/seccomp.rs`](../../sched/src/seccomp.rs), [`lib/seccomp-bpf`](../../lib/seccomp-bpf) | `seccomptest` |
| cgroups, bpf | [`cgroup.rs`](../../sched/src/cgroup.rs), [`bpfcg.rs`](../../sched/src/bpfcg.rs), [`kernel/src/bpf.rs`](../../kernel/src/bpf.rs) | `cgtest`, `bpftest` |
| Firefox sandbox | `ports/firefox/firefox.sh` | `ffsandbox.py` |

## See also

- [../userland/containers.md](../userland/containers.md) — what runs on top: rootless `podman`
- [../servers/security.md](../servers/security.md) — accounts, credentials and file-permission enforcement
- [../servers/networking.md](../servers/networking.md#network-namespaces) — per-namespace interfaces, veth, bridge, NAT
- [processes.md](processes.md) — `clone` flags, pid reaping
