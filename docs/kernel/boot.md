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

### Early boot on AArch64: a reachable UART, the HHDM fill, DTB reservations

- **Early exception vectors and UART (lane earlyuart, 2026-10-24).** `arch/aarch64/src/early.rs::setup` runs first in `kernel_main` on the Limine path. It installs the kernel vector table (`VBAR_EL1`, early flag set) so a fault before `arch_aarch64::init` is reported instead of spinning at `VBAR + 0x200`, and makes the physical PL011 reachable: an `AT S1E1W` probe decides whether the firmware's tables already map it; if not, and TTBR0 is 4 KiB/48-bit with an empty `L0[0]`, a static L1 table with one 1 GiB Device block is hooked in for EL1 only; failing both, early output is dropped (`EARLY_SILENT`) rather than faulting. `teardown()` removes the hook when the UART moves to its HHDM alias, so no process inherits a lower-half device map.
- **The HHDM fill is the default (lane hhdmfill).** Limine's HHDM covers only memory-map regions; the kernel fills the gaps. The long-standing "silent wedge" blamed on the fill was a stale TLB entry: on HVF the UART only "translated" through an entry left by the firmware's identity map, and the fill's `tlbi vmalle1is` removed it, so the next early print faulted recursively. `early.rs` now does `tlbi vmalle1` before the probe, and the fill's page-table pool is no longer placed where the DTB lives.
- **DTB reservations (lanes dtbres, dtblate).** `boot::device_tree::for_each_reserved_at` walks the header's `/memreserve/` block and every enabled `/reserved-memory` child (`reg` with the node's own cell sizes, `no-map` flag; dynamic `size`-only children are skipped). `parse()` stashes up to 32 ranges; `early_dtb_reserve()` in `main.rs` reserves each in the buddy allocator before `mm::init_with_map` and logs `[early] dtb reserved: <start>-<end> memreserve|reserved-memory/<name> [no-map]`. A DTB found only by the RAM search (the first 32 MiB of RAM in 4 KiB steps, `early_dtb_ram_scan`) is now parsed before the allocator as well; each page is probed with `AT S1E1R` first so an unmapped page is skipped instead of faulted on (`[early] dtb ram scan: found at <phys>|none (unmapped pages skipped: N)`). `no-map` ranges are also kept out of the HHDM on the Pi (lane pimem). The reserved-range table holds 64 entries (`MAX_RESERVED`).

### The kernel command line is 2048 bytes

One constant, `boot::COMMAND_LINE_SIZE` = 2048 (NUL included, as Linux on x86_64 and arm64), sizes the direct-boot buffer, the kernel's scrubbed copy behind `/proc/cmdline`, the per-driver knob copies and the kexec hand-over (`kexec/mod.rs` uses the same constant). It was 512 in the kernel copy, so a long `--append` silently lost its later knobs. A longer line is now cut at a token boundary and the cut is logged after the console is up (`[KERN] WARNING: cmdline truncated at N of M bytes`); `boot::cmdline_src_len()` keeps the length the bootloader gave. Guard: `cmdlinetest.py` on both architectures and a long `--append` in `kexecfdttest.py`.

### Choosing the root: `root=` is strict

The kernel never mounts the root (see above); it only decides which block device `/dev/root` is, in `drivers::blkdev::resolve_root`. With `root=` on the command line the answer is exactly that device or an error, never a guess:

- the accepted forms are `/dev/<name>`, `PARTUUID=<id>` and `LABEL=<f2fs label>` (`blockdev::root_form`); anything else (`UUID=`, a typo) is logged as `root=… is not understood` and refused, where it used to fall back to auto-detection;
- a `PARTUUID=` carried by two partitions is `Ambiguous` (`pick_unique`) and logged, not resolved by enumeration order;
- a value that matches nothing yet is `NotFound`, because the disk may still be enumerating; `init` retries every 100 ms for `rootwait=<seconds>` (default 30), saying so once a second, and with a named root it never falls back to another disk (the historical `/dev/vdb` fallback applies only when `root=` is absent);
- `root.fail=reboot` makes `init` wait 10 s (time to read the console), sync and restart after a root or `pivot_root` failure; the default is to park.

Without `root=`, resolution is the old chain (QEMU's `vdb`, else the unique removable or `sd*` f2fs volume that is not the home volume, ties broken by boot disk and partition type). On the Pi 5 the SD root is the partition with the F2FS superblock magic, not the first 0x83 partition, because dual-boot cards carry an ext4 0x83 as well. Why strict: an unrecognised or ambiguous `root=` used to be replaced by a guess (`vdb`, the lone removable f2fs volume), so a machine could boot a different disk from the one it was told to; a boot that cannot find its named root now says so and stops. Guard: `rootstricttest.py` (host test) and unit tests of the `rootspec` parser.

### Restart, power-off and the Pi 5 tryboot

`reboot(2)` ([kernel/src/power.rs](../../kernel/src/power.rs)) runs `shutdown_prepare` (one caller wins the `SYSTEM_RUNNING` transition, a second parks), then announces each phase on the console as `[POWER] phase N <name> at +X ms`, so a hang names its phase: 1 prepare (sync, f2fs clean-unmount checkpoint, NVMe queue delete and `CC.SHN`), 2 device quiesce, 3 machine prepare (console, persistent log, disks), 4 second quiesce (xHCI, bus mastering), 5 stop-other-cpus, 6 the arch's final PSCI/ACPI/reset step. After all flush phases and before the final step the other CPUs are stopped (Linux `smp_send_stop`: an NMI diverted to a park loop on x86-64, an SGI plus `IRQ_DIVERT` on aarch64), bounded at 500 ms and reported as `[POWER] other CPUs stopped: N of M in X us`, with the missing mask if some did not arrive. It is not done for kexec, which stops them itself. On aarch64 this matters under QEMU TCG, where parked CPUs no longer hammer MMIO while PSCI `SYSTEM_OFF` is processed (lane a64poweroff). An earlier design with a per-tick watchdog that forced the final action was dropped. The `reboot.*` command-line knobs (quiesce, reset method and type, HDA/xHCI reset, bus mastering) are documented at the top of that file's reboot-config block.

**One-shot boot partition and tryboot (Pi 5 only).** `reboot(LINUX_REBOOT_CMD_RESTART2, "[N] [tryboot]")` follows raspberrypi/linux: `N` goes into `PM_RSTS` and, when non-zero, into the PSCI `SYSTEM_RESET2` warm-reset cookie (`system_reset2_warm`, used only if `PSCI_FEATURES` reports it); a plain restart writes partition 0, the firmware default. `tryboot` sets the firmware `SET_REBOOT_FLAGS` bit, reads it back (`EIO` if it did not stick, and then no reboot happens) and notifies the firmware; a `tryboot` without `N` targets partition 1, where LeandrOS's `tryboot.txt` lives, so trials work when `autoboot.txt` defaults to Raspberry Pi OS on partition 3. The boot logs and clears a pending `PM_RSTS` request so a later watchdog or panic reset boots the firmware default. A trial boot (`leandros.tryboot=1` in the `cmdline_try.txt` the firmware read) starts a kernel thread that restarts plainly after `tryboot.deadline=<seconds>` (default 900) unless the trial is confirmed with the private reboot command `TBOK` (`reboot tryboot-commit`); a plain restart in a trial clears the one-shot flag, so the next boot reads `config.txt` and lands on the good slot. `TBHD` holds the deadline for up to a 300 s lease while a commit is writing the boot FAT.

### kexec: LeandrOS to LeandrOS

`kexec_file_load` and `reboot(LINUX_REBOOT_CMD_KEXEC)` replace the running kernel with another `kernel-direct` image without going through firmware, on both architectures. It was built in October 2026 (plan: `artifacts/notes/kexec/PLAN.md`) so that kernel updates on the real laptop and the Pi 5 do not need a stick or card swap, and so that kernel changes can be chained in QEMU. The code is `kernel/src/kexec/`; the front end is `/bin/kexec` ([userland/kexec/kexec.c](../../userland/kexec/kexec.c)), a kexec-tools-compatible subset: `-l KERNEL [--initrd=F] [--append=S] [--reuse-cmdline]`, `-u`, `-e`, or `kexec KERNEL ...` for load plus exec, and `-c CPU` to start the jump from a chosen CPU (a test aid).

**The target image is entered through the protocols it already speaks.** x86-64 multiboot2 (`EAX = 0x36d76289`, `EBX` = a built multiboot info, 32-bit, paging off) and the arm64 Image protocol (`x0` = a freshly written FDT, MMU off, EL1). No private hand-off struct exists, except two optional tags/properties described below.

**Load.** `kexec_file_load` reads the kernel and initrd from file descriptors into buddy pages described by a Linux-style indirection list (`stage.rs`); nothing is copied to its destination until the jump. Every destination page must be RAM in the running kernel's map, and staging and purgatory pages are never inside a destination nor below 1 MiB. `elf.rs` takes the ELF `p_paddr` segments; `info.rs` builds the multiboot info (x86) or the FDT (aarch64). The initrd goes to a fixed address (`0x4800_0000` on QEMU virt, `0x1000_0000` on x86-64 and on the Pi 5, which probes there). The image is staged under `/boot/kexec` by the image builder, and the regress suite runs `kexec -l ... && kexec -u` as a smoke.

**Quiesce, then jump (`power.rs::kernel_kexec`).**

1. Filesystems are synced, drivers' own shutdown hooks and the xHCI halt run, and the NVIDIA GSP can be unloaded first (`nvidia.kexec_unload`, opt-in, with an FLR fallback; lane gspkexec).
2. `drivers::dma_shutdown` hooks stop every bus master at its own registers (`Stage::Device`: the Pi 5 GEM and RP1 xHCIs, the dma40 engine), then `quiesce_all_pci`, then `Stage::Bus` hooks cut the bus (pcie2 `PERST#` and bridge reset). These run even with `reboot.quiesce=0`. The reason is a real corruption: a Pi 5 left RP1 mastering across a kexec, a live IPv6 frame landed on a free-list link in the next kernel's buddy allocator and the boot died on `[BUDDY] CORRUPT LINK`. Clearing bus-master enable in config space had not stopped it; stopping the engine did.
3. `kexec/devreset.rs` resets every PCI function once the other CPUs are stopped and the interrupt controllers masked: FLR, then AF-FLR (integrated Intel functions with no PCIe capability), then D3hot to D0, then a secondary bus reset only when every function behind that bridge is a reset-class endpoint and none is the boot framebuffer (`kexec.sbr=0` disables). Resets start together with one shared settle wait; each function's header (BARs, ROM, cache line, DevCtl/LnkCtl/DevCtl2) is saved and restored with memory decode on and bus mastering off. LTR, L1SS and AER state is restored too (lane kexecpm: after one kexec the three FLR'd endpoints of the laptop had LTR max latency `0`, which blocks deep package C-states; `/proc/pcipm` shows the registers). `kexec.devreset=0` skips the resets, `kexec.fbreset=1` also resets the framebuffer device and stops the framebuffer hand-over.
4. The secondaries are stopped (x86: the BSP sends INIT to every other started CPU, leaving each in wait-for-SIPI as the next kernel's INIT-SIPI-SIPI expects; a caller on an AP first NMIs the BSP, because INIT to the BSP would restart the firmware; aarch64: a park loop on a mailbox). The Pi 5 cannot use PSCI `CPU_OFF`: its TF-A reports the core off and then never restarts it, so secondaries flush their L1/L2 by set/way, post an arrival word and spin in the park page's mailbox loop, and the next kernel's `CPU_ON` answers `ALREADY_ON`, which counts as started when a mailbox write is posted (`kexec.cpuoff=1|0` overrides; lane pikexeccpu).
5. Purgatory (`purgatory_x86_64.s`, `purgatory_aarch64.s`) copies the segments and jumps. x86 clears PCIDE/LA57, sets `CR4 = PAE`, loads a null IDT and drops to 32-bit; aarch64 uses no SIMD, cleans the whole footprint including BSS, `ic iallu`, sets `SCTLR_EL1` and clears PAN.

**What survives the jump.**

- **Uptime stamps.** Console and klog lines carry a monotonic uptime prefix; `printk.time=0` on the command line turns it off.
- **The old kernel's last log.** From the start of `kernel_kexec` until the jump, every console line is also copied into a 16 KiB buffer allocated at load time (`prevlog.rs`). Its `(phys, size)` rides in a private multiboot2 tag (x86-64) or `/chosen/leandros,kexec-prevlog` (aarch64), never the command line, so nothing accumulates across generations or under `--reuse-cmdline`. The next kernel keeps the range out of its allocator (`reserve_early`), prints it as `[prevkexec] ...` lines once memory is up and frees it. This is how a machine without a serial port (the laptop) shows what the previous kernel did.
- **The boot FDT block (lane kexecfdt).** On aarch64 each generation's 16 KiB (FDT plus AP park page) is left out of `/memory`, so a chain used to gain one hole per generation until `do_load` refused images that spanned one. `kexec::return_boot_fdt()` runs right after `arch_aarch64::init` (the APs are past the park page): when `/chosen` carries `leandros,kexec-ap-mailbox`, it copies the recorded DTB to the heap for drivers, adds the block back as Available and frees it (`[KEXEC] boot FDT block A-B returned to the map (4 pages)`). `kexec.fdt_low=1` and `kexec.keep_fdt_hole=1` are debug knobs. The FDT also carries the non-coherent DMA mode (a Pi came up coherent after kexec before lane pigem-coh).
- **x86-64 uses all of RAM.** A direct (non-Limine) x86 boot used to map only 0-2 GiB; `direct_hhdm.rs` now gives `PML4[256]` its own PDPT and maps every Available/ACPI run above 2 GiB (1 GiB pages where possible and `pdpe1gb` exists, from a 256 KiB BSS pool), so a kexec'd or `-kernel` boot sees all memory.

**Status.** Chained kexec works on both architectures in QEMU (TCG, HVF and KVM; `kexectest`, `kexecfdt`, `kexecblk` suites), with SMEP/SMAP on, and on the real laptop. The Pi 5 flow (`remote.py pi-deploy`) depends on it ([remote.md](../development/remote.md#the-raspberry-pi-5-flow)); its DMA-quiesce and CPU-park fixes are verified by code and test but a given kexec on the real Pi still deserves a look at the serial log. Linux to LeandrOS and LeandrOS to Linux are not supported.

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

- **DTB reservations are honoured, but only for what the loader hands over.** `/memreserve/` and `/reserved-memory` are applied before the allocator (above); the x86 direct paths still reserve the kernel, initrd and MBI explicitly.
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

## EL2 / VHE (KVM builds only)

Default builds (Kconfig `KVM` off, cargo feature `drv-kvm` off) drop every entry
path from EL2 to EL1 as always: `check_el2` in `kernel/src/entry_aarch64.s` and the
AP stub in `arch/aarch64/src/smp.rs` assemble with `KVM_VHE=0`, so the `.if KVM_VHE`
blocks are not emitted and the instruction stream is the pre-KVM one.

With `drv-kvm` (`KVM_VHE=1`) and a CPU with VHE (`ID_AA64MMFR1_EL1.VH`), the BSP
and the APs stay at EL2 with `HCR_EL2 = RW|TGE|E2H` (+`API|APK` with PAuth) on both
the Limine path (rev 6 already enters at EL2 with E2H=1, MMU on) and the direct path
(MMU off). `CNTHCTL_EL2` is written in the E2H layout, `CPTR_EL2` opens FP/SVE/SME,
and `ICC_SRE_EL2` gets SRE|Enable. A CPU without VHE, or an MMU-on entry with E2H=0,
still takes the EL1 drop. The host tick moves from CNTV (PPI 27) to CNTHP (INTID 26,
GTDT "EL2 timer GSIV" when ACPI provides it); `cntp_*_el0` at EL2 is redirected to
CNTHP by E2H. The boot log prints `[el] running at EL2 (VHE)`. Test: `runtests kvmvhe`
(QEMU `virt,virtualization=on`, `LEANDROS_VIRT=1`) and `ktest=elcheck`.
