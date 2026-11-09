# lane mntcore (M0 of the mount-namespace design), 2026-11-09

Status: see RESULTS at the bottom. Design: leandros-artifacts/orch/designs/design-mntns.md.

## What landed
- `servers/vfs/src/mnt.rs`: `Mnt`/`MntNs` tree keyed by NsId (walk/translate with overmount
  top-of-stack and file binds, reverse, copy with ids remapped, locks and shared->slave on
  owner change, release/reap), the superblock registry under the hidden `/.leandros-sb`
  prefix, the LINKSCAN registry, and the per-thread-pid CALLCTX. `alias.rs` is now a shim
  over `mnt::ns0` (the old table, unchanged behaviour).
- `sched`: `Task.mntns`, `ForkOpts.mntns`, `ns::new_mntns` / `mntns_of` / `owner_user`,
  DYING + zombie bitmap + `ns::reap`, `mnt_active()` gate, `current_fs_ctx()` (root, cwd,
  mntns under one RUN_QUEUE lock), `current_mntns()` (0 without a lock while no mntns exists).
- kernel: `KPath` carries `mnt_id`/`ns_prefix_len`/`ctx_pid` + `Drop`; `KPath::from_ns`
  dispatches to `from_ns_mnt` only when `current_mntns() != 0`; `backing_walk_m`,
  `host_to_view` mntns branches; `resolve_ns` uses `current_fs_ctx` (1 lock instead of 2
  for relative paths) and answers ENOENT for `/.leandros-sb`.
- `/proc/mntselftest` (kernel selftest rows + `ns_live/ns_dying/sb_count`), `userland/mnttest`
  (merged with M8's ENOSYS case; `mnttest bench` = path-lookup microbench), `runtests.py mnt` suite.
- CLONE_NEWNS is still EINVAL; nothing creates a non-zero mntns yet, so every new branch is
  dead at runtime except through the selftest.

## Interfaces published for M1/M5/M6 and the overlay/devpts/mqueue/cgroup lanes
- `mnt::sb_create(kind, destroy) -> (SbId, backing_root)`, `sb_get/sb_put/sb_root/sb_dev/sb_count`.
- `mnt::register_linkscan(prefix, fn(backing, floor, follow) -> Option<(end_off, body)>)`, `mnt::linkscan`.
- `mnt::copy/translate/reverse/with_ns/release/reap`, `callctx_set/get/clear`.

## KPath::raw audit (for M1: a task with mntns != 0 can reach these)
- `open_kernel_path` (kernel-internal backing path), `/proc/<pid>/exe` (backing exe path),
  `fd_host_path` / `/proc/self/fd/N` (backing already): correct, already backing paths.
- mount(2) bind source (`syscall.rs` ~9207): `src` comes from resolve; in M1 it must be built
  with `from_ns` so the source is translated through the caller's tree.
- `read_user_cstr_kpath` (symlinkat target): verbatim by design (the link body is data, not a path lookup): correct.
- PT_INTERP / shebang: not `raw` sites; they reopen through `open_kernel_path`, so an
  interpreter path in a mntns task is resolved as a backing path. M1/M6 must translate it
  (the design's M0 audit item): left unchanged here because no task can have mntns != 0 yet.
