# lane/capaudit 2026-11-11: PARTIAL (kernel/sched/servers converted; vfs-tmpfs/xattr/f2fs Cred.host_root left)
Converted to host_capable / ns_capable_in (new sched::caps::host_capable_of / ns_capable_in_of for server code acting for a pid):
vt.rs (TTY_CONFIG), net SOCK_RAW (NET_RAW over netns owner userns), evdev serialkbd + block.rs (SYS_ADMIN), acct/vhangup/swapon/swapoff/kexec_load,
may_mount/newmnt (SYS_ADMIN), cred.rs cap_over/ptrace_cap/ipc_send_check (root bypass removed: CAP_KILL/CAP_SYS_PTRACE), proc link/environ/oom/comm,
nice/rtprio (SYS_NICE), prlimit (SYS_RESOURCE), setid_priv (SETUID/SETGID), NPROC bypass, PR_SET_MM, set_tid (CHECKPOINT_RESTORE/SYS_ADMIN), cgroupfs, sysfs/acpi writes.
Evidence: captest +7 cases (acct, vhangup, swapoff, mount, net_raw, kill, ptrace_link) PASS; two older captest cases changed to keep CAP_SETUID (Linux needs it for setresuid). No fail-first run on the old kernel (cases drop a cap as root, which host_root() could never refuse).
Regress: box x86_64 84 suites 0 bad; box aarch64 83 suites, only captest (test fixed, rerun PASS); Mac aarch64: ffsandbox on PASS, root podman bridge PASS, rootless podpasta pasta_portfwd FAIL identically on main baseline (not caused here).
Open: tmpfs chown/chmod, xattr, f2fs Cred.host_root (see TODO).
