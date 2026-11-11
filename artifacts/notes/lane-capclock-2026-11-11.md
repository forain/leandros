# lane/capclock 2026-11-11: FIXED
clock_settime/settimeofday (CAP_SYS_TIME), reboot/kexec_file_load (CAP_SYS_BOOT), module syscalls (CAP_SYS_MODULE),
iopl/ioperm (CAP_SYS_RAWIO), CLOCK_REALTIME_ALARM/BOOTTIME_ALARM timers (CAP_WAKE_ALARM) now use sched::caps::host_capable instead of host_root().
Evidence: captest sys_time_needs_cap FAIL on the unfixed kernel (aarch64), PASS after the fix; full box regress on both arches.
Open: remaining host_root() callers are listed in TODO.md (Podman / namespaces section).
