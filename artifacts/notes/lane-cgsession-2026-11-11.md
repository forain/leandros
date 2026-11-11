# lane/cgsession 2026-11-11 - FIXED

Item: only /bin/login joined the session cgroup; sshd, greetd and the greeter launcher did not.
- Shared helper `leandros_libc::cgsession::join`, used by /bin/login and greeter-launch.
- sshd (userland/sshd/src/cgsession.rs): session scope created before fork, child joins, scope removed when the session ends.
- greetd: via pam-leandros pam_open_session.
- No autologin code exists in the tree, so that part of the TODO item had nothing to fix.
- Test: sshsmoke `session_cgroup` (15bede00, committed before the fix).
- Evidence: green on fix (Mac HVF): `ssh_session_cgroup: PASS out='0::/user.slice/user-1000.slice/session-sshd5.scope'`; red on 15bede00 (box): see lane report.
- Full aarch64 box regress on lane/cgsession: see lane report.
