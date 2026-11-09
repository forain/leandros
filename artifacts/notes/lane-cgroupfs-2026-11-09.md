# lane/cgroupfs (CG2 cgroupfs) - 2026-11-09

Result: cgtest 20/0 aarch64; full box regress both arches 74 suites, 1 bad = nstest cgns_in_userns
(known main failure from cgcore x idcore, lane cgnsfix; not cgroupfs). ffsandbox on=True.
Guard branch lane/cgroupfs-guard showed the tests fail pre-fix.

## Design choices
- The cgroup2 mount flag is sticky and global: once mounted, it stays set for all namespaces.
- The view root is the reader's cgns root (paths are shown relative to it).
- chown of a control file is a no-op: files inherit the directory owner.
- mountinfo shows only the initial-ns line.
