# lane nsforge (2026-11-10)
FIXED: H1 of idflip-review-A (/.leandros-ns/<kind>/<id> openable by name).
- resolve_ns: ENOENT for /.leandros-ns (typed paths); only the mount translation of a bound nsfs file reaches the VFS synthetic branch.
- hold_checked(kind,id): refuses id >= NS_MAX, free/dying slots, and slots of another class (per-slot SLOT_KIND).
- put: last ref is 1 -> DYING by CAS; PARENT/OWNER_EUID/EGID/LEVEL cleared before the slot becomes free.
- Fail-first on main (nstest, box x86_64): nsforge_detached_theft_denied, nsforge_freed_slot_cycles, nsforge_live_other_user_enoent FAIL; nsforge_oob_id_enoent (cat /.leandros-ns/0/300) panics the guest (suite timeout). All pass after.
- Regression guard nsforge_bound_nsfs_open_and_setns passes before and after.
- Box regress both arches: 81 suites, 0 bad. ffsandbox on (Mac aarch64) passes. Rootless podman run --network=none alpine:3.21 echo hi (idmap=test) prints hi.
