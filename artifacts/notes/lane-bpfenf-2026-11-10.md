# lane/bpfenf: cgroup device-program enforcement (2026-11-10)

Status: FIXED (device programs attached via bpf(BPF_PROG_ATTACH, BPF_CGROUP_DEVICE) are now enforced).

## Enforcement points
- `sched/src/bpfcg.rs` `dev_check(pid, is_blk, major, minor, acc)`: one atomic load while no program is attached; evaluates the program of the caller's cgroup, Err(-EPERM) on deny.
- `servers/vfs/src/lib.rs` `handle_open`: hook placed before the O_PATH-skipping check, using `open_dev_identity(path)` to resolve the device node (covers devnode_open re-entry). O_PATH opens are skipped (no I/O access).
- `servers/vfs/src/lib.rs` `handle_mknod`: hook at the top (acc = MKNOD); covers sys_mknodat, overlay and proxy paths. Whiteout (0:0) is exempt.

## Known gap
- `faccessat` on a device node is not checked.

## Evidence
- bpftest `enforce` case: with enforcement disabled (TEMP `if true {return Ok(())}`, never committed) bpftest fails `enforce` (fails=1); with the real code 11/11 pass (aarch64 Mac HVF).
- ffsandbox.py aarch64 mode on: RESULT True.
- Box full regress x86_64 and aarch64: see merge commit report.
- crun container run in-guest was not done (no crun harness reproducible from the tree); not a blocker for the kernel path.
