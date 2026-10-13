# Conventions

The standing rules for contributing to LeandrOS. Most exist because breaking them once cost real time; the reason is given with each.

## Release builds only

Always build and test **release** targets (`./scripts/build-all.sh` builds nothing else). Debug builds hang or crash early in boot — they need far larger kernel stacks, and their symbols desynchronise from the running image. Never use a debug build for testing, and do not trust a result obtained on one. See [building](../getting-started/building.md#release-builds-only).

## Limine boot protocol revision 6 or newer

The kernel's Limine requests require base revision **6** at minimum. Never downgrade the revision, under any circumstances — including as a workaround for a bootloader problem. When Limine 11.4.1 wedged on FEAT_LPA2 under TCG, the fix went into the QEMU CPU model (`-cpu max,lpa2=off`), not into the protocol revision. See [boot](../kernel/boot.md).

## Both architectures, after every change

Every change is built and run on **x86-64 and AArch64** in QEMU before it is called done:

```sh
./scripts/build-all.sh
./scripts/run-qemu.sh aarch64
./scripts/run-qemu.sh x86_64
```

plus the relevant regression suites ([testing](testing.md)). If one architecture could only be tested on a slow accelerator, say so explicitly with the result. Report results by accelerator (HVF, KVM, TCG), not only by architecture ([methodology](methodology.md#the-accelerator-is-not-the-architecture)). For changes to early boot, MMU/cache setup, linker scripts or locks reachable before userspace, QEMU is not enough evidence for real hardware ([methodology](methodology.md#qemu-cannot-catch-arm-hardware-bugs)).

## Rust-only

The kernel, servers, drivers and in-tree programs are Rust. Do not commit C sources for new probes or tests, and do not teach the build system a C compilation path to make an import convenient. A C probe that has delivered its finding stays outside the repository (its build script may be kept as documentation); a capability worth keeping is rewritten as a `no_std` Rust crate under `userland/` and joins the existing regression suites — permanent tests belong there, not in a side directory. `artifacts/.gitignore` blocks `*.c`/`*.h` so a careless drop cannot reintroduce them.

The C that does exist is there because it *is* the interface: ABI shims for third-party C libraries (`ports/input-stack/shims`, `ports/openrm/glue`, port glue for Mesa, PipeWire and Firefox), vendored third-party files, and `userland/privtest`, `userland/nettest` and `userland/syscalltest`, which are C so the identical source can be checked against a real Linux kernel ([testing](testing.md)). The complete list of tracked C files is kept in [userland](../userland/README.md).

## Linux semantics win

When LeandrOS and Linux behave differently, LeandrOS changes — error codes and their ordering included. Do not patch ported software to accommodate the kernel, and never patch COSMIC: the one exception (`ports/cosmic-comp/0001-toplevel-info-weak-window.patch`) was approved individually for one leak. COSMIC renders on the GPU; do not tune for, or patch around, software-rendering cost. See [design principles](../overview/design-principles.md).

## Kernel code

The full list is in [kernel invariants](../kernel/invariants.md). In short:

- No bare `static mut` outside the boot parsers and arch stubs; wrap shared state in `spin::Mutex` or atomics.
- Never touch user memory while holding a scheduler spinlock; copy through `AddressSpace::read_user_buf`/`write_user_buf` outside it.
- Use checked arithmetic at memory boundaries; round time conversions up; program timers as absolute deadlines.
- Keep kernel stack frames small: the build fails above 48 KiB. Large structures are built in place (`Box::new_uninit()`), because `Box::new(Struct { .. })` materialises the struct on the stack first; compile-time size assertions guard the largest ones.
- Every `ENOMEM` goes through `enomem_site()`.
- New debug output is gated behind a constant that defaults to off. After gating a helper, look for local copies of it and diff a live serial log before and after.
- MMIO register accesses are volatile (`addr_of_mut!(…).write_volatile(…)`). Non-volatile writes to adjacent fields of a packed register block let LLVM merge them into a store-pair instruction that HVF cannot decode.

## Tests

- Show a new guard failing on the unfixed code before trusting it passing, against a prediction written in advance ([testing](testing.md#show-a-new-guard-failing-before-you-trust-it-passing)).
- Run suites that matter on fresh images.
- A result is quoted with its date, commit, architecture and accelerator; numbers measured before a fix to the measuring substrate (such as the 2026-09-16 clock fix) are stale.

## Commit hygiene

- Commit messages describe the change and its reason in the project's voice: a subsystem prefix (`sched:`, `drm:`, `ports/firefox:`, `notes:`) and a subject that states what was wrong or what now holds — for example `futex: FUTEX_WAIT_BITSET takes an absolute deadline, not an interval`.
- Keep history clean and professional. No tool, assistant or session attribution in commit messages, trailers or authorship; check before pushing.
- Merge lanes one at a time into an integration branch and build the merged tree on both architectures before `main` moves ([workflow](workflow.md#lanes-and-waves)).
- Record investigations in `artifacts/notes/lane-<name>-<date>.md`; anything finished is deleted from [TODO.md](../../TODO.md), not marked done — `git log` is the record of what happened.
- Use the latest major versions of any GitHub Actions a workflow references.

## Worktrees and shared state

- Development worktrees are siblings of the repository root (`../leandros-<name>`), never nested; the build resolves sibling repositories relative to the tree.
- Never run `build-all.sh` while a QEMU booted from the same tree is running.
- Give each concurrently driven QEMU its own `LEANDROS_RUN_ID`.
- Keep the brush sibling on its pin (`ports/brush/sync.sh apply ../brush`).

See [workflow](workflow.md).

## Documentation

This handbook describes the current design; dated history goes in [history](../overview/history.md), and investigation records stay in `artifacts/notes/`. When a change alters a design decision, update the handbook page that states it, with the evidence.

## See also

- [Workflow](workflow.md)
- [Testing](testing.md)
- [Design principles](../overview/design-principles.md)
