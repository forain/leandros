# lane/rev3fix (2026-11-08)

FIXED: H1 (proc magic-link access gate), N1, N2.

Commits: 5f1a2355 (H1 N1 N2), e61b5465 (chdir follows /proc magic links through the gate),
dd11a044 (chdir via magic link skips the walk permission probe).

Evidence (box, by hash):
- Failing-first: variant 00997de1 (without H1) failed nstest magic_link_access.
- 5f1a2355: full x86_64 regress, only failure nstest magic_link_access line 141:
  chdir("/proc/<root pid>/cwd") gave ENOENT instead of EACCES (aarch64 the same).
- e61b5465: EACCES now right, but own chdir("/proc/self/cwd") hit EACCES (line 148) because
  the open probe re-checked search permission on a cwd under a root-only directory.
- dd11a044: nstest RC=0 on x86_64 and aarch64; full x86_64 regress: see below.

Fix: sys_chdir calls follow_magic_final for /proc/<pid>/{cwd,root} (EACCES for other users'),
then sets cwd directly without the VFS open probe.
