# lane/kexecexec (2026-10-19)

Lane 3 of `artifacts/notes/kexec/PLAN.md`: `reboot(LINUX_REBOOT_CMD_KEXEC)`
boots the image staged by `kexec_file_load`. **aarch64 only**; on x86_64 it
returns EINVAL (nothing loaded) or EOPNOTSUPP (loaded).

## Path
- `power.rs::kernel_kexec`: `kexec::exec_prepare` (EINVAL if nothing loaded;
  root is checked by `sys_reboot`), then the restart shutdown (sync, f2fs
  clean unmount, NVMe CC.SHN, audio, e1000e, klog flush, disk flush, xHCI
  halt + HCRST, no VBUS off), then `quiesce_all_pci`: every non-bridge
  function gets virtio status=0 (common cfg found via the vendor cap,
  polled back to 0), MSI/MSI-X enable off and bus master off.
- `kexec/exec_aarch64.rs::prepare` (task context): purgatory page + TTBR0
  identity map (L0 + one L1 of 1 GiB blocks, every GiB holding RAM, Normal
  WB from the MAIR index found at runtime, AP=EL1 RW) in pages outside every
  destination; TCR for TTBR0 = 48-bit/4K/WB walks, IPS widened to PARange.
- `stop_and_jump`: arm `arch_aarch64::exception::IRQ_DIVERT` (handle_irq
  EOIs, then calls it), SGI 1 to every other CPU. CPU 0 runs `final_cpu0`:
  timers off, AFFINITY_INFO until every other CPU is OFF (2 s cap),
  `gic::kexec_quiesce` (all enables/pending/active cleared in distributor and
  every redistributor, groups off, ICC_IGRPEN1=0), then TTBR0 idmap, WXN off,
  branch to the purgatory. A non-zero caller CPU powers itself off and CPU 0
  takes over (seen in a chained run: "stopping 3 other CPU(s) from cpu1").
- APs (`park_off`): timers + CPU interface off, then into the purgatory
  page's `kexec_purgatory_off`: MMU/caches off, VBAR=0, PSCI CPU_OFF.
- `purgatory_aarch64.s` (no SIMD/FP, no stack, PIC): EPD1 on, walk the
  indirection list, `dc civac` the whole footprint (BSS, initrd, FDT
  included), `ic ialluis`, SCTLR_EL1=0x30d00800, PAN 0, x0=FDT, x1-x3=0.
  The new kernel is entered at EL1 (at_el1 path; PSCI conduit stays HVC).

## Bugs found on the way
- **aarch64 direct boot never took an interrupt correctly**: the direct
  linker script defined `__exception_vectors = .` before `*(.text*)`,
  overriding the asm symbol, so VBAR_EL1 pointed at the AP stub (0x...800)
  while the real table sat at 0x...1000. Every IRQ landed in garbage:
  `[EXC] Unexpected Exception! ... ELR=<sched::ap_entry>`. This is the
  "aarch64 --direct crashes in virtio-gpu submit" of lane bootparse.
  Fixed in `linkers/aarch64-direct.ld`.
- A physical x0 DTB was parsed but not remembered (`remember_dtb` keeps only
  higher-half addresses), so `/cpus` fell back to the Aff0 1..7 guess. The
  direct path now switches to the HHDM alias.
- The kexec'd kernel needs its initrd: the image now carries
  `/boot/kexec-initrd` (= `initrd-$arch.cpio`).

## Test
`kexectest -e [EXTRA]`: load `/boot/kexec` + `/boot/kexec-initrd` with this
boot's `/proc/cmdline` (+ `kexec.gen=N+1`), `sync`, `reboot(KEXEC)`.
- **TCG (uefi-tcg, 4 CPUs)**: Limine boot -> kexec -> kexec (chained): each
  generation logs `[early] DTB at 0xFFFF8000...`, `[early] cmdline:
  kexec.gen=N`, `[SMP] 4 CPUs online`, reaches `login:`; root login works,
  `/proc/cmdline` = `kexec.gen=2`, `nproc` = 4, plain `kexectest` ALL PASS
  in generation 2.
- **HVF**: generation 1 boots to `login:` and userspace, but on **1 CPU**:
  `[SMP] mpidr=0x1 accepted CPU_ON but never came online`. gdbstub shows the
  AP never reaches the new kernel's stub: after CPU_OFF it resumes in the
  purgatory's post-hvc wfi loop (PC = end of `kexec_purgatory_off`), with
  x0 = 0, i.e. QEMU HVF loses the CPU_ON entry PC for a vCPU that powered
  itself off. Not fixed (QEMU side, or needs a different AP hand-off).

## Open
- x86_64 exec (lane follow-up): INIT APs, 32-bit purgatory, MBI, and the
  OVMF NVS overlap at 0x800000 (load refused with EADDRNOTAVAIL there).
- HVF AP restart after CPU_OFF (above).
- `userland/kexec` tool (lane 4) and a runtests suite entry for the chain.
- Real hardware untouched: no GPU/NIC device reset beyond virtio + BME/MSI.
