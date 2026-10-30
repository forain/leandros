# lane/kmskapi (K2a) 2026-10-30 - DELIVERED

- drivers/src/kms_class.rs: repr(C) KKms{Backend,Flip,Cursor,Outputs,Pipe2}Ops + static Rust adapters (const-generic cursor slots for pipe A/B), `register_raw(kind, ops, ctx)`; kinds 0..5 (5 = pipe-B cursor, register before pipe2).
- drivers/src/drm_class.rs: size-gated KDrmOps tail (mmap_pin, proc_text, prime_dedup, ptecheck_mmap, ptecheck_fd_to_handle); dispatch fns resolve through the registered table at use time. mmap_pin installs BACKEND_MMAP_PIN -> adapter at module registration. tmpfs census stored kernel-side (set_tmpfs_census/tmpfs_census).
- i915_stub.rs forwards proc_text/prime_dedup/ptecheck_*/set_tmpfs_census to drm_class.
- Exports (+5, 118 -> 123): kapi_kms_register, kapi_kms_set_internal_panel, kapi_drm_hotplug, kapi_ext_present_stats, kapi_tmpfs_census (the last is listed under K2b in the plan; done here because the census hook is part of the late-registration risk - K2b should skip it). RegKind::KMS = 14. kmod-kapi `drm::{kms_register,...}` wrappers + kmod-abi sys decls.
- ktest kmodkapi: literal 118 replaced by `ksymtab().len() == NEXPORTS`; new k2a cases drive every adapter with fake tables of each kind and the KDrmOps tail. Note: ktest feature gate for i915-real is `drv-i915` (not `i915`).
- Bug caught by ktest: cursor2 slot index (kind - KIND_CURSOR = 3) hung the CPU; fixed.
- Evidence: ktest=kmodkapi aarch64 1 pass 0 fail (123 exports); box aarch64 regress 61 suites 0 bad; x86_64 release build ok.
- Not done: kmod-abi mirrors of the KMS POD types live in drivers/kms_class.rs only (module side K3a must mirror them); kmodplat exports not yet merged, so recount after its merge: NEXPORTS is the array length, ktest derives from it.
