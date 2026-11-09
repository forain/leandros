# lane/idsig (design-idmap lane 13, hazard H6) - 2026-11-09

Status: FIXED (see verification at the end).

## What changed
Signal, ptrace-class, prlimit, setpriority and IPC-send permission now use the Linux rule:
capability in the TARGET's user namespace (host root still passes), or a same-uid match.
New helper `cap_over(c, t, cap)` in `sched/src/cred.rs` (a `Caller` snapshot carries its userns
and effective caps, because RUN_QUEUE is held and `caps::ns_capable_in` would deadlock); it calls
`caps::capable_snap(caller_ns, caller_euid, caller_caps, target_ns, cap)`: true when the caller
holds the cap in its own ns and that ns is the target's ns or an ancestor, or it owns a ns whose
parent is its own. A cap in a sibling or child ns gives no power.

## Security: every check changed (userns used = TARGET task's userns in all cases)
| site | before | after | why that ns |
|---|---|---|---|
| `may_signal` (kill/killpg/kill(-1)/tgkill paths) | host root only | host root OR CAP_KILL over target userns | Linux `kill_ok_by_cred` ns_capable(tcred->user_ns, CAP_KILL) |
| `same_owner` (setpriority, sched_setscheduler admission) | host root | host root OR CAP_SYS_NICE over target userns | Linux `set_one_prio_perm` |
| `prlimit` read/write another process | host root | host root OR CAP_SYS_PTRACE over target userns | Linux `check_prlimit_permission` |
| `ipc_send_check` | host root | host root OR CAP_SYS_PTRACE over the port owner's userns | rule is ptrace_may_access |
| `ptrace_attach_check` (pidfd_getfd) | host root | host root OR CAP_SYS_PTRACE over target userns | ptrace_may_access ATTACH_REALCREDS |
| NEW `ptrace_read_check`, called by vfs for open of `/proc/<pid>/ns/*` | none | own tgid, host root, CAP_SYS_PTRACE over target userns, else fsuid/fsgid == target real/eff/saved and target dumpable; else EACCES | Linux proc_ns_get_link -> ptrace_may_access(READ_FSCREDS); closes the setns-lane gap |
Unchanged on purpose: prlimit raising a hard limit and `can_nice` stay host-root (Linux checks
CAP_SYS_RESOURCE / CAP_SYS_NICE in the init ns there). Same-uid comparisons remain on kuids
(Linux compares kuids after translation).
Only grants were added (over-strict -> Linux-correct); the one new denial is the ns-file open.

## Tests (userland/idmaptest)
kill_subuid_capkill_ns, kill_host_same_kuid_denied (ns root with CAP_KILL in N1 cannot kill or
renice a host-root task whose numeric id equals its in-ns uid 0), nsroot_opens_ns_of_mapped_user,
prlimit_ns_child (also setpriority; host root denied), ns_open_needs_ptrace_access (uid 1000
opening /proc/1/ns/net gets EACCES; /proc/self/ns/net ok). Knob cases SKIP without idmap=test.
Pre-fix run (box aarch64, idmap=test, tests-only commit): kill_subuid_capkill_ns,
prlimit_ns_child, ns_open_needs_ptrace_access FAIL. The other two are over-grant guards and pass
before and after.
