# lane kmodL3a (2026-10-29): build-id, .ksymtab, module window - DELIVERED

- build-id: `--build-id=sha1` in both kernel RUSTFLAGS; note + empty `.ksymtab` placed with explicit AT() in the x86_64, x86_64-direct, aarch64 scripts. aarch64-direct (no PHDRS, rpi5 derives from it) folds both into `.rodata` because lld gives a standalone SHT_NOTE its own PT_LOAD there (5 loads vs 4); `llvm-readobj --notes` therefore does not list that kernel, `__build_id_start/_end` and the boot log do. PT_LOAD layout, `_start` (+0x40) and Image header address unchanged; kernel-direct.bin +4096 B (code growth).
- x86_64-direct.ld: ASSERT image end < 256 MiB.
- mm/src/kmodwin.rs: constants re-exported from kmod-abi::layout (256 KiB slots); init (TCR check, no-leaf probe, tables), reserve/release bitmap, map/unmap; RX is kernel-only executable via new PageFlags::KERNEL_ONLY_X (UXN on aarch64); `paging::leaf_pte` (arch_leaf_pte in both arches).
- direct_hhdm::unshare_kernel_slot() -> bool, called BEFORE extend(); failure disables kmod, never panics.
- kernel/src/kmod/{mod,buildid}.rs; `kmod::init()` after kva::init in kfacil_init (main.rs).
- ktest=kmodwin (opt-in, in EXTRA): PASS aarch64 Limine, x86_64 Limine, x86_64 direct. aarch64 direct receives no cmdline, so only the `window ... ok` log line was checked there.
- kexec gen1 (aarch64): window ok in gen1, login reached. aarch64 default regress: PASS.
- Not done: rpi5 build/.bin size (inherits the aarch64-direct script via sed), x86 direct /bin/vfstest (userland reached login only).
- Aside: HHDM alias of module pages stays RW+X on direct kernels (pre-existing); W^X holds for the window only.
