# lane/devptsref 2026-11-09

FIXED. devpts instances were first-free slot ids (1..16) with no mount-copy reference, so after an ns0 umount a
namespace copy could map a later instance that reused the id (cross-session/container pty access).

Fix: an instance is now a counted superblock (`mnt::sb_create("devpts")`); its id is the monotonic sb id (never
reused), the backing dir /dev/.devpts/<id> is dynamic (no static 1..16 table; one leaked 'static path per
instance), `ns0_snapshot` sets `Mnt.sb` to the instance so `mnt::copy` takes a reference, `alias::add/remove`
get/put it, and the instance is freed (devpts::INST entry dropped) with its last mount. Failed mounts discard.
pty instance ids widened u16 -> u32. Open ptys never reference a reused id, so they stay valid until close.
Limit stays 16 live instances (ENOSPC).

Tests (userland/mnttest): devpts_copy_keeps_instance, devpts_no_reuse_after_umount. Box regress x86_64 and
aarch64 PASS; Mac aarch64 ptytest/pstest/jobtest 0 fails. Not run against unfixed main (on main umount destroyed
the instance, so the child's <mnt>/ptmx open fails -> first case fails by construction).
