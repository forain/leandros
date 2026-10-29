# lane kmodL5a (2026-10-29): kapi PCI/mm/entropy exports, kmod-kapi shim, kenv seam

DELIVERED. Branch lane/kmodL5a.

- 8 new kapi exports in kernel/src/kmod/exports.rs (ksymtab 14 -> 22, const signature checks): kapi_pci_find / pci_cfg_read / pci_cfg_write / map_device / phys_to_virt / page_alloc / page_free / hwrng_register. map_device accepts only KMAP_* and always maps kernel-only, non-exec; bad cfg width/offset reads as all ones; page_alloc uses `buddy::alloc_below` when `below != 0`.
- kapi_hwrng_register: 4-slot trampoline table (const-generic `hw_tramp::<N>`), `register_source`, `note_registration(owner, HWRNG)`, then new `sched::random::reseed_now()` (try_lock on GATHER, logs `[RANDOM] reseeded (late source) from: ...`).
- kmod-abi: KmodDevId (16 B), KPciDev, KMAP_*, sys fn aliases + extern decls.
- lib/kmod-kapi (workspace member): `pci`, `mm::{paging,buddy}`, `sched::{monotonic_ns,random}` with the Y signatures; `set_owner(KmodId)` must be called by the module runtime (kmod_rt) from kmod_init; random::register_source keeps the Rust fn in a 4-entry static table behind an extern "C" trampoline. Compiles for both kernel targets with build-std.
- Seam: drivers/src/kenv.rs (`pub use ::mm; ::sched; crate::pci`), virtio_rng.rs now `use crate::kenv::{mm, sched, pci}` and `pci::` paths (5 sites).
- ktest=kmodkapi (opt-in EXTRA).

Evidence: symbol sets (llvm-nm, hash and `.NNN` stripped) main vs lane on aarch64 differ only by the new kapi_*, hw_tramp, reseed_now, ktest items, note_registration (now out of line) and two inliner-noise items in kexec (Vec<Seg> clone / Drain drop). virtio_rng symbols unchanged. `[RNG] virtio-rng bound` on aarch64 and x86_64. ktest kmodload+kmodkapi PASS on aarch64 and x86_64 (Limine/UEFI). aarch64 regress PASS.

Deviations: KPciDev is 40 bytes (plan said 48; the listed fields sum to 40), asserted as 40. Owner plumbing into kmod-kapi is left to kmod-rt (L4a)/L5b via `kmod_kapi::set_owner`.
