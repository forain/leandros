# lane pasta (R1) - PARTIAL
Commits on lane/pasta: nobody user, ptrace_read_check userns relaxation + nstest nobody_opens_child_ns (114/0 on aarch64).
pasta now gets past user setup and the ns open; blocked at setns EBADF (CLONE_FILES across vfork children). Details: orch/designs/podtrace6-findings.md "pasta".
