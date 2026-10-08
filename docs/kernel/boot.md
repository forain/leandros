# Boot

How a LeandrOS kernel gets from firmware to the first user process on both architectures: the Limine path (the default everywhere), the direct paths (QEMU `-kernel`/PVH and the Raspberry Pi firmware), the exact order of `kernel_main`, and the hand-off to userspace `init`. The one hard rule: **the Limine base revision is 6 and is never lowered.**

## Overview

| Path | Used by | Entry state | Boot information |
|---|---|---|---|
| **Limine (UEFI)** | x86-64 and AArch64 under QEMU (default `run-qemu.sh` mode), USB sticks on a real PC, the RPi 5 via the vendored `RPI_EFI.fd` | Long mode / EL1, paging on, kernel linked at `0xffff_ffff_8000_0000`, HHDM supplied | Static Limine request structs in the image |
| **Direct, AArch64** | `run-qemu.sh --direct` (QEMU `-kernel`), RPi 5 firmware loading `kernel.elf`, QEMU `raspi4b` | MMU off, maybe EL3 or EL2, `x0` = DTB (or 0) | Flattened device tree, or fixed QEMU `virt` fallbacks |
| **Direct, x86-64** | `run-qemu.sh --direct` (PVH entry) | 32-bit protected mode | multiboot2 if present, else a fixed QEMU memory map |

The kernel ELF carries headers for all of them at once: Limine requests, a multiboot2 header, a multiboot1 header and a Xen PVH note on x86-64 ([kernel/src/entry_x86_64.s](../../kernel/src/entry_x86_64.s)); an arm64 Linux `Image` header on AArch64 ([kernel/src/entry_aarch64.s](../../kernel/src/entry_aarch64.s)). The linker scripts are in [linkers/](../../linkers): `x86_64.ld` and `aarch64.ld` link at `0xffff_ffff_8000_0000` for Limine; the `-direct` variants link at `0xffff_ffff_8000_0000` (x86-64, loaded at 1 MiB) and `0xffff_8000_0000_0000 + 0x4008_0000` (AArch64 QEMU `virt`; `0x0020_0000` on `--rpi5`).

## How it works

### The Limine protocol and the revision-6 invariant

`kernel_main` lives in [kernel/src/main.rs](../../kernel/src/main.rs). The image declares `limine::BaseRevision::with_revision(6)` and requests the HHDM, the memory map, a framebuffer, modules (the initrd), the RSDP, the executable address, the DTB, the executable command line and the executable file (used to identify the boot disk). Responses are parsed by [boot/src/limine.rs](../../boot/src/limine.rs) into a `boot::BootInfo` ([boot/src/lib.rs](../../boot/src/lib.rs)). The configuration is [limine/limine.conf](../../limine/limine.conf) (protocol `limine`, `KASLR: no`, 1920x1080x32); `build-all.sh` pins Limine 11.4.1.

**Minimum Limine base revision is 6. Do not lower it.** Several things in the kernel depend on revision-6 semantics, and lowering the revision does not fail loudly — it changes what the loader maps:

- Revision ≥ 1 does **not** map MMIO into the HHDM. The kernel therefore maps every device it touches explicitly: the LAPIC and framebuffer on x86-64 ([arch/x86_64/src/lib.rs](../../arch/x86_64/src/lib.rs)), the PL011, GIC distributor/CPU interface/redistributors and PL031 on AArch64 ([arch/aarch64/src/lib.rs](../../arch/aarch64/src/lib.rs)), PCI ECAM, and everything `ioremap` hands out ([memory.md](memory.md#kernel-mappings-device-memory-and-the-kva-window)).
- The x86-64 PAT analysis rests on what Limine 11.4.1 writes to `IA32_PAT` (WC at PA5) and on the shape of the HHDM it builds ([arch-x86_64.md](arch-x86_64.md#pat-identical-on-every-cpu)).
- The boot-disk identity is read from Limine's `struct limine_file` at the protocol's own offsets, because the `limine` crate 0.6.3's struct is wrong for this revision (it omits two words and its GUID type is not `repr(C)`); verified against a raw dump on a USB stick, 2026-10-05 (`d39f97c`).

The initrd is an uncompressed newc CPIO archive. It is shipped in the FAT image as `initrd.gz` (a historical name) and loaded as a Limine module; the kernel looks for `bin/init` in it ([kernel/src/init.rs](../../kernel/src/init.rs)).

### AArch64 entry

`_start` in [kernel/src/entry_aarch64.s](../../kernel/src/entry_aarch64.s):

1. **Park secondary cores** by `MPIDR_EL1.Aff0` (QEMU `raspi4b` releases all four cores at once; an earlier `LDAXR/STLXR` first-core-wins election livelocked under TCG's exclusive monitor with four contending vCPUs, so the gate is a purely local MPIDR read).
2. **Drop EL3 → EL2** if needed (QEMU `raspi4b` resets into EL3).
3. **Drop EL2 → EL1** if needed (QEMU `-kernel` with `-cpu max`, and the RPi 5 firmware, enter at EL2; Limine enters at EL1). Before the drop it records `boot_entered_el2` (QEMU then exposes PSCI on the SMC conduit instead of HVC), opens `CNTHCTL_EL2` so EL1 can read the counters, and sets `ICC_SRE_EL2.Enable` when the CPU has a GICv3 system-register interface so EL1's `ICC_SRE_EL1` access does not trap into EL2 vectors that are never installed. `0x9` (SRE|Enable) is not an AArch64 logical immediate: it is built with `mov` + `orr`.
4. **Limine path (MMU on):** translate addresses through the executable-address response and continue at EL1 with the loader's tables.
5. **Direct path (MMU off):** zero BSS first (the early page tables live in BSS), build `early_pgtables` (identity + HHDM 1 GiB blocks for 0–4 GiB, Normal memory above 4 GiB up to 16 GiB, Device blocks for the BCM2712 UART/GIC window), program MAIR and TCR, then enable the MMU **and** the caches together.
6. Install `VBAR_EL1`, call `kernel_main(dtb_ptr)`.

Two lessons from the first real Pi 5 boots are encoded in step 5 and documented in place:

- **The table walker must be cacheable.** The TCR value sets IRGN/ORGN to write-back and SH to inner-shareable. With a non-cacheable walker and `SCTLR_EL1.C` set, every mapping created after the caches come on is invisible to translation: `handle_page_fault` installs a correct PTE, reports success, and EL0 re-faults on the same instruction forever, silently. QEMU never showed it.
- **Caches must be on before the first lock.** With `SCTLR_EL1.C` clear every access is Normal non-cacheable, and the exclusive monitor can fail `STXR` permanently on non-cacheable memory, so the first `spin::Mutex::lock()` (`mm::buddy::reserve_range`) never completed on a Cortex-A76. TCG implements exclusives regardless of cacheability, which is why the direct path "worked" in emulation for years.

The history of getting AArch64 to EL0 at all — the switch from the physical to the virtual timer, page-table access through the HHDM, the dynamic `UART_BASE`, ELF cache maintenance — is the historical record [design/aarch64-boot-investigation.md](../design/aarch64-boot-investigation.md). Its "blocking issue at `eret`" is long resolved.

### x86-64 entry

Under Limine the CPU arrives in long mode with paging on at `_start` (`cli`, then the common path into `kernel_main(0)`; Limine boot data comes from the static request structs, not an argument). The PVH entry `_start_pvh` brings up the serial port, builds a GDT and identity/HHDM page tables for the low 2 GiB, enables long mode (setting `EFER.LME` only — `NXE` is set later by the kernel), and joins the same path with the multiboot2 information pointer. (`kernel/src/x86_64_start.rs` is an unused alternative naked-function entry; it is not a module of the kernel crate.)

### `kernel_main`, in order

The order matters and differs from the older README flow charts in one important way: **the memory manager comes up before arch init on both architectures**, because arch init needs the buddy allocator for the page tables it builds when it maps devices.

1. **Parse boot information.** Limine: `boot::limine::parse_with_requests`. Direct AArch64: validate `x0` as a DTB, otherwise scan 1–2 GiB of RAM for the FDT magic (QEMU `-kernel` with an ELF enters with `x0 = 0` but still loads a DTB), then `boot::device_tree::parse` ([boot/src/device_tree.rs](../../boot/src/device_tree.rs)); failing that, a fixed 1 GiB map at `0x4000_0000`. Direct x86-64: `boot::multiboot2::parse`, else a fixed map below 2 GiB.
2. **Direct paths only: reserve what the map does not.** A DTB calls *all* RAM available, unlike Limine, which marks the kernel and modules reserved. The kernel image (`KERNEL_PHYS..__bss_end`), the initrd (at a fixed physical address placed by `run-qemu.sh`'s `-device loader`, or `0x1000_0000` on the Pi) and, on the Pi, the VideoCore framebuffer are passed to `mm::buddy::reserve_range` (8 slots). On `rpi5` the PCIe ECAM base is zeroed: BCM2712 publishes an ECAM node, but scanning it without link training produces asynchronous external aborts that arrive, all at once, when the scheduler first unmasks `DAIF.A` to enter userspace — implicating an innocent `init`.
3. **`mm::init_with_map`** — HHDM offset, buddy (minus the low 1 MiB), per-call-site page tags, slab, atomic pool ([memory.md](memory.md)).
4. **Arch init** — `arch_x86_64::init` (PAT, GDT/TSS, IDT, SSE and `CR0.WP`, LAPIC/framebuffer mapping, MADT CPU discovery, APIC, IOAPIC, timer calibration, SYSCALL MSRs, SMP start-up) or `arch_aarch64::init` (FP/SIMD access, MAIR index 2, SCTLR EL0 cache bits, UART/GIC/RTC/framebuffer mapping, vectors, GIC, timer, PSCI SMP). APs come up last and park in `sched::ap_entry` until the BSP opens the scheduler. See [arch-x86_64.md](arch-x86_64.md) and [arch-aarch64.md](arch-aarch64.md).
5. On x86-64, ACPI FADT power registers and the PS/2 controller probe.
6. **`time_init`** (wall-clock epoch from the RTC) and **`kfacil_init`** (the kernel VA window's top-level entry must exist before the first user page table copies the kernel half; ACPI MCFG ECAM on x86-64).
7. AArch64 only: if Limine supplied no DTB-derived UART/ECAM, search RAM for a DTB, then fall back to ACPI MCFG. This runs after arch init because the DTB parser's struct zeroing used to emit SIMD instructions before FP was enabled.
8. **PCI** — map bus 0 of ECAM (AArch64: as Device memory; it is not in the HHDM) and scan.
9. **Framebuffer console** — the bootloader framebuffer if there is one; otherwise (the normal AArch64 `virtio-gpu-pci` case, which has no GOP) bring up virtio-gpu and create a scanout-backed surface. Then `tty_server::vt::init`. See [graphics/console.md](../graphics/console.md).
10. **Seed the CSPRNG** (virtio-rng, `/chosen/rng-seed`, CPU generator, jitter) before any user process exists.
11. **`init::init_task_main`** — next section.

### `init_task_main` and the hand-off

[kernel/src/init.rs](../../kernel/src/init.rs), still on the boot stack, single-threaded:

1. `ipc::init` (registers the task-exit hook that releases a dying task's ports), the file-backing hooks for demand-paged exec, and the exit-teardown hook that frees fds/pipes/sockets/TTY state on every path out of a task.
2. **In-kernel servers:** VFS, evdev (+ virtio keyboard), DRM, PipeWire. Their port numbers are recorded for auxv ([ipc.md](ipc.md#auxv-service-discovery)).
3. Tick hook and per-CPU deadline hook for poll/epoll/timerfd deadlines; the monotonic clock source for `sched` ([time.md](time.md)).
4. **Block devices** and their VFS backend, the boot-disk identity (the `/dev/root` tie-break), **USB** (xHCI, USB mass storage registered as `sdX` before userspace looks for its root), the read-only NVIDIA probe, the **persistent log**, then **networking** (virtio-net, `net_server`, and the `net_daemon` kernel task).
5. **Load `/bin/init` from the initrd** into a fresh address space (static ET_EXEC, bias 0), map a 1 MiB demand-paged stack ending at `0x1000_0000`, spawn it as PID 1 and record it as the reaper of orphans.
6. Start the system work queue (`[kworker]`), the `request_firmware` backend, the `ktest` thread if requested; enable per-CPU console staging; **`sched::run()`**, which opens `SCHED_ONLINE` and releases the parked APs.

The filesystems are **not** mounted by the kernel. Userspace `init` starts the servers it owns, waits for and mounts `/dev/root` (and `/data`, `nofail`), `pivot_root`s, and starts the graphical login and the serial getty, each supervised and respawned independently:

```
init (PID 1) → mount / (f2fs on /dev/root) and /data
             → graphical login (default; skipped if /etc/leandros/text-login exists):
                 greetd → cosmic-comp (kiosk) → greeter-launch
                        → cosmic-greeter → authenticates → cosmic-session → COSMIC desktop
             → serial getty loop (always): /bin/login → authenticate against /etc/shadow
                        → setresuid/setresgid → brush → (optionally) start-cosmic manually
```

The userspace half is documented in [desktop/login.md](../desktop/login.md), [getting-started/logging-in.md](../getting-started/logging-in.md) and [userland/programs.md](../userland/programs.md).

## Design decisions

### Limine everywhere, with the direct paths kept alive

*Choice:* boot through Limine on every supported target, including the RPi 5 (via a vendored UEFI firmware), and keep a direct path per architecture.
*Why:* one boot protocol means one memory-map shape, one HHDM, one framebuffer hand-off and one module loader across QEMU, real PCs and the Pi. The direct paths are kept because they are the only way to boot when firmware is the thing under test — the RPi firmware loading `kernel.elf` directly, and QEMU `-kernel` for bisecting a bootloader-independent problem.
*Cost:* every direct path must re-do what Limine does for free: reserve the image and initrd, supply fallback MMIO addresses, and zero BSS before building page tables in it. Most of the boot-path comments in `kernel_main` are about exactly that.

### Fixed QEMU fallbacks are gated by board

When neither a DTB nor ACPI is available the kernel assumes QEMU `virt`: PL011 at `0x0900_0000`, PCIe ECAM at `0x40_1000_0000` (the `-cpu max` high window). Those fallbacks are compiled out of `rpi5`/`raspi4b` builds, where the UART base is a compile-time constant and `raspi4b` has no PCIe window at all — probing a phantom ECAM there reads unbacked memory instead of taking the safe `ecam_base == 0` early return.

### Reserve the first megabyte, always

`mm::buddy` never hands out memory below 1 MiB, whatever the map says. The OVMF x86-64 map lists `[0, 0x87000)` as usable; before this rule a 512 KiB block sat at physical address 0, which is also the free lists' null-link sentinel — the block behind it became unreachable and "frame 0" was reported as out-of-memory. The same megabyte holds the AP start-up trampoline at `0x7000` and real-mode firmware structures. Linux reserves it for the same reasons (`5f48843`).

## Limitations and open work

- **DTB `/memreserve/` entries are not honoured.** The FDT header's reservation-map offset is parsed but unused; only `/memory` nodes are read, and the direct paths reserve the kernel, initrd and Pi framebuffer explicitly. An older README line claimed otherwise. On a board whose firmware reserves RAM through `/memreserve/` or `reserved-memory` nodes, those frames would be handed to the buddy allocator. The Pi 5 boots today because its firmware's reservations do not overlap what the kernel uses, not because of this code.
- **AArch64 direct boot under HVF hangs** (vCPU spinning at PC `0x200`, no serial output) — an upstream QEMU PL011/HVF timer-starvation class, retested on QEMU 11.0.2. UEFI/Limine under HVF works and is the default; the launchers force TCG for direct boot.
- `entry_aarch64.s`'s direct-boot park loop still elects the boot CPU on `MPIDR.Aff0` (the `raspi4b` path only); dense CPU indices are assigned later, at SMP start-up ([scheduler.md](scheduler.md#cpu-bring-up-and-dense-cpu-indices)).
- `KASLR` is off.

## Code and tests

| What | Where |
|---|---|
| Entry assembly | [kernel/src/entry_x86_64.s](../../kernel/src/entry_x86_64.s), [kernel/src/entry_aarch64.s](../../kernel/src/entry_aarch64.s) |
| `kernel_main`, Limine requests, console staging | [kernel/src/main.rs](../../kernel/src/main.rs) |
| Kernel init task | [kernel/src/init.rs](../../kernel/src/init.rs) |
| Boot-information parsers | [boot/src/limine.rs](../../boot/src/limine.rs), [boot/src/device_tree.rs](../../boot/src/device_tree.rs), [boot/src/multiboot2.rs](../../boot/src/multiboot2.rs), [boot/src/acpi.rs](../../boot/src/acpi.rs) |
| Linker scripts | [linkers/](../../linkers) |
| Limine configuration | [limine/limine.conf](../../limine/limine.conf) |

Every change to boot is verified by booting **both** architectures to a login prompt (`./scripts/run-qemu.sh aarch64`, `./scripts/run-qemu.sh x86_64`); the regression suites then run on that boot ([development/testing.md](../development/testing.md)). Boot diagnostics worth knowing: `[MEMMAP]` prints the whole map once (the first question of every memory-corruption post-mortem), `[ARCH] MAIR_EL1 before=… after=…` and `[ARCH] IA32_PAT before=… after=… wc=1` prove the attribute setup on a live register, and `[SMP]`/`[GIC]` lines report CPU enumeration and GIC version detection.

## See also

- [arch-x86_64.md](arch-x86_64.md), [arch-aarch64.md](arch-aarch64.md) — what arch init sets up
- [memory.md](memory.md) — what `mm::init_with_map` builds
- [hardware/raspberry-pi-5.md](../hardware/raspberry-pi-5.md) and [hardware/pc.md](../hardware/pc.md) — real-hardware boot
- [getting-started/running.md](../getting-started/running.md) — boot modes and accelerators
- [development/conventions.md](../development/conventions.md) — the Limine revision rule as project policy
