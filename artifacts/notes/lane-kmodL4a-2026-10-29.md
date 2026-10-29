# lane kmodL4a (2026-10-29): Rust module build pipeline

DELIVERED. Branch lane/kmodL4a.

- `lib/kmod-rt` (header/ids statics, `module!`, kmod_init, allocator, panic, kinfo!/kwarn!/kerr!; calls `kmod_kapi::set_owner`), `modules/hello`, `modules/SLOTS`.
- `scripts/kmod/kmod-build.sh <arch>`: cargo staticlib -> rust-lld with `kmod.ld` per kernel variant (kernel, kernel-direct) -> `kmodpost.py` -> `kmodcheck` (loader's `validate()`); stages `target/kmod/<arch>/{initrd,rootfs}/lib/modules/<id>/{<name>.kmod,modules.alias}`. No-op when nothing to build. `$KMOD_EXTRA` adds modules, `$KMOD_AUTOLOAD` appends to `modules.load`. build-all runs it after testblob; testblob no longer does `rm -rf`.
- Guards (kmodpost.py): dependency allowlist (cargo tree), kapi-only undefined symbols (llvm-nm), no defined symbol of a kernel crate (legacy+v0 mangling), aarch64/x86 SIMD scan (llvm-objdump), W^X/shape (PT_LOAD R,RX[,RW], no TLS/dynamic/interp/exec stack), link ASSERTs for .got/.plt/.init_array/.fini_array/TLS and slot size. Header patch: build-id, slot, init, ids, flags (PERMANENT; SOFTFLOAT_CHECKED on aarch64). Alias lines from `.kmod.ids`.
- `scripts/kmod/selftest.sh` (host): badsym, baddep, badsimd, badwx, init_array all rejected by the named guard; hello passes.
- Needs llvm-nm/llvm-objdump (PATH or /opt/homebrew/opt/llvm/bin).

Evidence: hello autoloads and logs "hello from rust", vec/format, spin held count=1, `/proc/modules` `hello 262144 0 - Live <slot0>` on aarch64 Limine, aarch64 direct (own build-id dir), x86_64 Limine. aarch64 regress PASS with KMOD_TEST=1 KMOD_EXTRA=hello. Slot-too-small link fails with "module outgrew its slot reservation".

Deviations: PT_LOAD set may be R,RX (no RW) for a module with no writable data. kmod-rt depends on kmod-kapi (set_owner). `.bss`-less hello has a 0x18-byte RW segment. Kernel sha equality vs main (G1) not measured; no kernel source touched. Not run: x86 after merging L5a (module-side change only).
