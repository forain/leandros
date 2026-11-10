# lane/pastaleak (2026-11-10) - REFUTED as a kernel bug: no missing inotify event

- pasta (passt pasta.c:488) calls `inotify_add_watch(fd, netns_dir, IN_DELETE)` on the DIRECTORY of the --netns path; if inotify_init1/add_watch fails it falls back to a timer. With the old stub inotify (vfs.inotify=0) nothing ever fires, so pasta lives forever.
- With `vfs.inotify=1 idmap=test` (aarch64 HVF, podpastatest, PODPASTA_CHURN=10): churn fails=0, no pasta process left, /run/user/1000/netns absent, /run/netns empty. The existing tmpfs IN_DELETE hook (VFS_UNLINK) already covers the pattern; no inotify.rs change needed.
- Harness leftovers (not leaks): pasta_leaks "mounts: 1" is a baseline `netns` mountinfo line (the /run/netns mount), pasta_portfwd FAIL (container httpd missing in image; pre-existing).
- Leak stays with the default knob (vfs.inotify=0) until lane L5 flips it.
- Duplicate check: tunbe::loop_own (bridgeip3, all vif kinds, runs first) subsumes tunbe::local_loop (podpasta, tap vifs, runs after on the remaining frames). local_loop is redundant; not refactored here.
