# lane/procbind 2026-11-15 -- podman run broken since 21a9e8ab (/proc/interrupts)

Cause: two VFS gaps for generated /proc files (meminfo, uptime, stat, loadavg, cmdline, mounts, interrupts, ...):
1. `stat` answered ENOENT (they are not RAMFS entries; only cpuinfo/version were), so mount(2) of a bind onto one failed in kpath_is_dir.
2. An O_PATH/read fd on one is a `/tmp/.psys_<idx>` snapshot, so `/proc/self/fd/N` named that synthetic path; crun mounts onto `/proc/self/fd/N`.
Fix (servers/vfs/src/lib.rs): PROC_SYSTEM_FILES stat arm; PSYS_ORIGIN table (snapshot idx -> /proc path) used by handle_fd_path. Pruned on each insert (a first version capped at 64 entries and broke podbridge after the cap).
Tests: mnttest procbind_fd_names_proc_file, procbind_all_files_{ns0,newns} (red before: errno 2 on 7 files); new regress entry host:podsmoketest.py (`podman run --rm alpine echo hi`, loads /alpine-oci*.tar if needed, skips without it).
Why regress missed it: no podman run in the regress list (podbridge etc. are opt-in suites). podsmoke is now in regress; the box ships the alpine archive, so it runs there.
Gate: box regress + podbridge PASS on aarch64 and x86_64 (f17df8af), post-merge mnttest+podsmoke PASS on aarch64.
