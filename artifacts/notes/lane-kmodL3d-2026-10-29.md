# lane kmodL3d (2026-10-29): module syscalls, /proc/modules, cmdline, initrd lookup, linked test module

DELIVERED (see gates below).

- `kernel/src/kmod/sys.rs`: init_module (copy_from_user, max 16 MiB), finit_module (lseek + `read_fd_upto`, as kexec does), delete_module (-EBUSY). Root only, else -EPERM; also -EPERM when kmod is disabled. params ignored. `arch_kmod_report` feeds `/proc/modules` (servers/vfs `gen_modules`, PROC_DIAG_FILES): `<name> <size> <regs> - <Live|Failed|Loading> 0x<base>`.
- `kernel/src/kmod/autoload.rs`: `kmod=0` (calls `kmod::disable()`), `modules.load=a,b`, `modules.blacklist=x,y`, `load_by_name` (initrd `lib/modules/<build-id>/<name>.kmod` via `extract_binary_from_initrd`), `boot()` called from init.rs after `netdev::init`. The blacklist is also enforced inside `load_image`, so syscall loads honour it.
- Extra: if the cmdline has no `modules.load=`, `lib/modules/<build-id>/modules.load` in the initrd is the default list. Needed because aarch64 direct `-kernel` boots receive no command line. Default builds ship no such file.
- `kapi_log` now prints `[kmod <name>] <level> msg` while that module's init runs (`load::CUR_OWNER`); it prints plain `[kmod]` outside init.
- `scripts/kmod/ksyms.py <kernel-elf> <out.ld>`: pure-python ELF read; unique `kapi_*` names, count == ksymtab length; writes PROVIDE lines plus `<out>.buildid`.
- `scripts/kmod/testblob.sh <arch>` + `testblob/{kmodtest-*.S,kmodtest.ld}`: assembly module, one build per kernel variant (standard, direct) into `target/kmod/<arch>/initrd/lib/modules/<id>/{kmodtest,kmodbad}.kmod`; kmodbad carries the other variant's id. Runs only with `KMOD_TEST=1`.
- `build-all.sh`: `create_initrd` now runs after `build_kernel` and copies `target/kmod/<arch>/initrd/` when present.

## Veneer result (aarch64)
Module text at 0xfffffe000ff8xxxx calls kapi_log/kapi_ktest_ping at 0xffff8000_40257b58 (HHDM). lld emits `__AArch64AbsLongThunk_kapi_*` (`ldr x16,.; br x16`), no fallback needed. Booted and loaded on aarch64 direct `-kernel` and Limine.

## How to write a module (until L4's kmod-build exists)
1. Header (128 B `KmodHeader`, `lib/kmod-abi`) in `.kmod.info` at the slot base, R-only first PT_LOAD: magic, abi 1, machine, build-id of the target kernel, slot_base/size (256 KiB multiples inside the window), `init` VA, 32-byte name.
2. Code: `extern "C" fn kmod_init(ctx: *const KmodCtx) -> i32` (>= 0 = Live). Call only `kapi_*` symbols (functions, call-only).
3. Link static, non-PIE, with `ksyms.py` output as an input script (`--no-undefined`), separate R / RX / RW PT_LOADs, page aligned. See `testblob/kmodtest.ld` and `testblob.sh`.
4. Install as `lib/modules/<build-id hex>/<name>.kmod` in the initrd; load with `modules.load=<name>` or `finit_module`.
L4 must keep `-C relocation-model=static` and no `-pie`, or lld picks ADRP thunks and every kapi call fails to link.

## Not done / notes
- Gate 6 (userland syscall tool: EEXIST, -EPERM for non-root) deferred to L4's kmodctl; syscalls only compile-checked and exercised via kernel paths. delete_module is a one-liner (-EBUSY).
- aarch64 direct cannot test kmodbad/blacklist/kmod=0 (no cmdline); done on Limine. x86 direct `-append` is fixed to `root=/dev/vda` in run-qemu/driver.
- Unrelated: build-all's f2fs step fails in this worktree with "stale greetd ... lacks sha512-crypt rounds= marker" (shared greetd-lane artifact); the images were cloned from main.
