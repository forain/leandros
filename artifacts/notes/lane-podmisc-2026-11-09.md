# Lane podmisc (2026-11-09) - FIXED

Three small pod-compat gaps.

- execveat (x86_64 322, aarch64 281): implemented through `sys_execve_kp`, shared with execve (AT_EMPTY_PATH, dirfd-relative, AT_SYMLINK_NOFOLLOW -> ELOOP, follow symlink).
- aarch64 /proc/cpuinfo: arm64 fields (BogoMIPS, Features, CPU implementer/architecture/variant/part/revision).
- Standard default PATH (/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin) in login, shell, sshd, init and the cosmic start script (was /bin only).

## Tests
- userland/exectest: 6 execveat cases; userland/proctest: cpuinfo_arm64_fields; sshsmoke.py: session_path_standard.
- Fail-first on main + test files only (box, aarch64): exectest 7 fails (execveat_* cases), proctest cpuinfo_arm64_fields "missing BogoMIPS".
- Full regress on lane/podmisc, box, both arches: see merge commit / below.
