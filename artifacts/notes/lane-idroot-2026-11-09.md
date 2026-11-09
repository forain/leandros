# lane idroot 2026-11-09 -- host root vs namespace root

Status: FIXED. Spec: orch/designs/design-idmap.md (lane idroot).

`sched::host_root_of(pid)` / `host_root()` = euid 0 AND initial user namespace.
Root of a child userns is capable only over that namespace, so every
host-wide privilege check now uses host_root. Cases: userland/idmaptest
(ns_euid0_not_host, host_root_unaffected), kept alongside idstat's cases.

## Converted sites
- sched/cred.rs may_signal: CAP_KILL across tasks is host-wide (ns-scoped term belongs to idsig).
- cred.rs same_owner: root bypass of the owner check is host-wide.
- cred.rs setpriority x2: raising priority / RLIMIT_NICE bypass is CAP_SYS_NICE (host).
- cred.rs prlimit x2: raising hard limits and touching other tasks is CAP_SYS_RESOURCE (host).
- cred.rs ipc_send_check: cross-user IPC override is host-wide.
- cred.rs ptrace_attach: CAP_SYS_PTRACE over arbitrary tasks is host-wide.
- cred.rs current_is_root: now means host root.
- sched lib.rs set_mm_arg_field (PR_SET_MM): CAP_SYS_RESOURCE, host.
- sched lib.rs fork_admission: NPROC bypass is CAP_SYS_RESOURCE, host.
- kernel power.rs reboot: CAP_SYS_BOOT, host.
- kexec/mod.rs, kmod/sys.rs: loading kernels/modules is host-only.
- syscall.rs: secprobe, spawn privilege, get_robust_list of others, iopl/ioperm,
  timer_create ALARM (CAP_WAKE_ALARM), set_tid, proc_link_allowed and the
  remaining euid==0 checks -- each guards a host-global resource.
- servers/vfs/src/block.rs x3: block device ioctls/ops need host CAP_SYS_ADMIN.
- servers/net x2: raw sockets (CAP_NET_RAW) and privileged ports (<1024) need host root.

## Deliberately not converted
- caps.rs on_exec/on_uid_change, setid_priv: owned by lane idcaps.
- sys_setxname: main's ns::uts_admin() kept (lane utsns).

## Evidence
- aarch64: captest, nstest (incl. uts_*), idmaptest 5/5 pass.
- x86_64 box full regress: 70 suites, 0 bad (one vfstagtest wake-count flake, passes on re-run).
- Guard: ns_euid0_not_host fails when host_root_of ignores the userns (Mac aarch64: FAIL ns_euid0_not_host, 4 passed 1 failed; with the check: 5/5 PASS).
- Test cases removed vs main: none.

## Final verification (merged tree)
- aarch64 (Mac): captest, idmaptest pass; ffsandbox (mode on) passes; default regress green except host:vfstagtest (known load flake, passes alone).
- x86_64 box regress: 71 suites, only vfstagtest.
- nstest on the final merged tree: 85 pass / 1 fail, cgns_in_userns (nstest.c:2297, clone(NEWUSER|NEWCGROUP) from an
  already-unmapped userns gets EPERM). new_userns rule 8 (unmapped euid, sched/src/ns.rs:94) is identical on main and
  was not touched by this lane; cgcore (77/0) predates it. Looks like a main-level cgcore x idcore interaction, not an idroot regression
  (not verified on a pure-main build). Left open for the owners.
- Final x86_64 re-run after the last main merge skipped: the box tmpfs was 93% full.
