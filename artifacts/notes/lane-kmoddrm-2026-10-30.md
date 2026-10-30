# lane kmoddrm (W2-7a, partial): DRM class kapi, 2026-10-30

PARTIAL. Done: uapi half of (a). Not done: KMS tables, i915 Y-extraction, (b) I915=m, (c) autoloadtest i915 case.

Done
- drivers/src/drm_class.rs: repr(C) `KDrmOps` (mirror of gpu_uapi::UapiBackend) + `register`; a static adapter UapiBackend forwards to the table (ioctl/gem_close: >=0 result, i64::MIN not ours, -errno).
- kernel/src/kmod/exports/drm.rs: kapi_drm_register, drm_offset_alloc/release, fence_wait, syncobj_install/exists/in_fences, fence_irq_hint, kms_event_complete (NEXPORTS 97 -> 106). RegKind::DRM = 13. kmod-kapi::drm wrappers.
- ktest kmodkapi: new cases, PASS aarch64. autoloadtest iwlwifi x86_64 PASS (compiles kmod-kapi). Box default regress x86_64 PASS; aarch64 see report.

Open (next steps)
- KKmsOps mirror of gpu_kms (KmsBackend, KmsFlip, KmsCursor, KmsOutputs, KmsPipe2) + kapi_kms_register; kapi_boot_fb, kapi_backlight_register, kapi_page_ref_many.
- i915 uses mm/sched/arch/ksync/hrtimer/uaccess directly (no kenv): needs a kenv seam, then modules/i915, Kconfig tristate, pci alias 8086 display, i915::{emit,now_ns,delay_us} relocation.
- On merge with kmodstor: recount NEXPORTS from the table (ktest has literal 106).
