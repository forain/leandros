# lane/memfdexec 2026-11-09 — FIXED (crun cloned-binary re-exec)

Blocker #1 of podtrace2-findings: `crun --version` printed "Failed to re-execute libcrun via memory file descriptor".
Test: userland/memfdtest (crun's sequence, in the regress list). On main it failed 5 steps.

Broken steps (kernel/src/syscall.rs, servers/vfs/src/lib.rs):
- F_ADD_SEALS after F_SEAL_SEAL succeeded (must be EPERM); F_SEAL_WRITE/GROW/FUTURE_WRITE never enforced (write, grow-by-ftruncate).
- fexecve = execveat(memfd, "", AT_EMPTY_PATH) returned ENOENT: fd_host_path names no usable path for a nameless memfd. Now exec'd via /proc/self/fd/N identity route.
- /proc/self/exe after a memfd exec read "/proc/self/fd/N" and only worked because memfd_create ignored MFD_CLOEXEC. Now the image pins the memfd (TMP_FDREFS), readlink gives "/memfd:<name> (deleted)", open("/proc/self/exe") reopens the sealed memfd (via /proc/.memfd-exe/<tgid>), pins released lazily when the tgid stops running a memfd image.
- memfd_create now honours MFD_CLOEXEC, records its name, mode 0777.
Result: memfdtest PASS, `crun --version` = 1.30.1 in the guest (aarch64).
Open: MFD_ALLOW_SEALING absent => Linux starts sealed with F_SEAL_SEAL (not done); pin is not inherited by fork children.
