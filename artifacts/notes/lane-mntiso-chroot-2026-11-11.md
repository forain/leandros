# lane mntiso-chroot (E4) - 2026-11-11 - FIXED

- Bug: `ns_resolve_links` re-anchored an absolute symlink body at the mount-ns root and floored `..` at the ns root,
  ignoring the task's chroot. A non-root jailed process in a mntns could `ln -s / esc` and read host files.
- Fix: `ns_resolve_links(ns, path, follow, floor)` - `floor` is the caller's chroot prefix (`sched::current_root_arc()`,
  "/" when not chrooted). Absolute link bodies re-anchor at `floor`; `..` never climbs above it. Callers:
  `rewrite_one` (servers/vfs/src/lib.rs) and `ns_follow_links` (kernel/src/syscall.rs, exec path). No check loosened.
- Tests (mnttest, userland/mnttest/mnttest.c): `chroot_ns_abs_link`, `chroot_ns_dotdot_link` (open of host sentinel
  /root/mntiso-secret through the link must be ENOENT), `chroot_ns_exec_through_link` (in-jail `/l -> /m` exec works).
- Fail-first (box aarch64, test-only commit): all 3 FAIL. With fix: mnttest fails=0 (Mac aarch64 HVF).
