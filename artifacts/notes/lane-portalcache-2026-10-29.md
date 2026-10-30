# lane/portalcache 2026-10-29 - FIXED

Cause: ports/portal/build-backend.sh put $HERE/out/stub-$arch and $HERE/out/sdk-$arch (worktree paths)
into the cargo rustflags (link-args). rustflags are part of cargo's unit hash, and CARGO_TARGET_DIR is shared,
so every worktree produced a full second set of hashes (verified: lane worktree + old-script worktree = 4 portal
files/3526 deps vs 2/1763).
Fix: SDK/stub dirs and the --config file are reached via symlinks under $CACHE/portal-stable/<arch>/ (refreshed each
build), so flags are worktree-independent. Guard: if deps holds >3 xdg_desktop_portal_cosmic .d files, wipe
deps/.fingerprint/build (final binary kept).
Evidence: after wipe, build from lane worktree, then from a second worktree: "Finished in 0.71s", 1 hash (2 files), 1763 deps.
Cache size: portal-target 2.5G after one full build (was 236G). busd-target 857M.
Other scripts: busd/build.sh uses a shared busd-target but its rustflags are path-free (static reloc only) - fine.
Other COSMIC builds (m6-session-bins) not using CARGO_TARGET_DIR in ports/*; not changed.
aarch64 regress PASS. No in-session portal smoke beyond regress (binary same size as before).
