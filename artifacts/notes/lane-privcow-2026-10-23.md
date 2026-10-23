# lane/privcow 2026-10-23 -- FIXED (P0 data corruption found and closed)

Task 1: on main (b371d9ed) the read-only >256 MiB MAP_PRIVATE tmpfs alias was a MAP_SHARED VMA
over the VMO frames. mprotect(PROT_WRITE) + a store wrote straight into the file: child stores
after fork leaked to the parent and the file (read() and a MAP_SHARED view), hole writes too.
memtest `big_private_cow` (300 MiB, mprotect path) FAILED on main: 3 markers + hole leaked.

Fix: mm `AddressSpace::map_private_vmo_frames` (shared body with map_shared_frames):
MAP_PRIVATE, cow=true region, PTEs installed read-only, frames pinned by the VMO acquire so
the pageref is >1 and the ordinary CoW promotion copies on first store. mprotect and clone_as
already treat cow regions correctly. `install_vmo_fault` installs cow regions read-only.

Task 2: with CoW working, a WRITABLE MAP_PRIVATE of a >256 MiB tmpfs file takes the same
path (syscall.rs `tmp_private_big_ro` no longer requires PROT_READ) instead of EINVAL.
memtest `big_private_cow` covers both (mprotect and direct PROT_WRITE): fork child store,
parent store, file via pread, file via MAP_SHARED map: all PASS. memtest/forktest/shmtest
and regress green on aarch64 (pstest tty + nettag2test flake are pre-existing).

Open: MAP_SHARED > 256 MiB of tmpfs is still EINVAL. truncate of the file zaps CoW'd private
copies of the aliased range too (they refault from the object; Linux would keep them).
