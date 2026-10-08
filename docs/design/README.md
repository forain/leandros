# Design records

Design records are the working documents written *before* (or while) a subsystem was built: surveys, option analyses, and implementation plans. They are kept verbatim as historical evidence of why the code looks the way it does. Line numbers and "current state" claims inside them describe the tree at the time they were written — the handbook chapters linked below describe the system as it is now.

| Record | Written for | Current description |
|---|---|---|
| [K2 — event-loop layer](k2-epoll-design.md) | Making `epoll`/`poll`/`select` block on a wait queue instead of yield-spinning; `signalfd4`, `inotify`, `/proc/self/exe` | [Kernel › Event loop](../kernel/event-loop.md) |
| [K3 — dynamic binaries in the ELF loader](k3-elf-design.md) | `PT_INTERP`, `ET_DYN` at a load bias, and the auxiliary vector musl's dynamic linker needs | [Kernel › Processes](../kernel/processes.md) |
| [K4 — DRM, synthetic sysfs, evdev tablet](k4-drm-design.md) | The DRM ioctl surface Mesa `kms_swrast` and GBM need, a read-only `/sys` skeleton, and an absolute pointer | [Graphics › DRM and KMS](../graphics/drm-kms.md), [Input](../input.md) |
| [AArch64 boot investigation](aarch64-boot-investigation.md) | Getting the AArch64 target from Limine hand-off to a shell: virtual timer, HHDM page-table access, UART relocation, I-cache maintenance | [Kernel › Boot](../kernel/boot.md), [Kernel › AArch64](../kernel/arch-aarch64.md) |

Investigation notes for individual work lanes — the measurements each claim in this handbook rests on — live under [`artifacts/notes/`](../../artifacts/notes/).

## See also

- [Handbook home](../README.md)
- [Development › Methodology](../development/methodology.md) — how records like these are written and checked
