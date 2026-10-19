# lane/kexecx2apic (2026-10-19)

Goal: run x86_64 kexec with the local APICs in x2APIC mode. Every earlier
run (TCG, KVM 4 vCPUs, the laptop) stayed in xAPIC.

## What existed

x2APIC support was already in the kernel. No bug was found in it.
- `apic.rs`: MSR access (0x800 + off>>4), ICR as one 64-bit MSR write with an
  mfence/lfence before it, 32-bit IDs, no delivery-status polling.
  `init` picks the mode once on the BSP: x2APIC if IA32_APIC_BASE.EXTD is
  already set (firmware, or a kexec hand-off), or if `cpus::need_x2apic()`
  (an MADT CPU with APIC ID > 254) and CPUID.1:ECX[21]. Each AP follows the
  BSP. An AP left in x2APIC under an xAPIC BSP goes through the disabled
  state to xAPIC.
- `cpus.rs`: Local x2APIC MADT entries, 32-bit IDs. `own_apic_id()` and the
  AP trampoline at 0x7000 read the ID from CPUID leaf 0xB (fallback leaf 1),
  never from the MMIO ID register, so an AP's APIC mode does not matter.
- kexec (`exec_x86_64.rs`): NMI to the BSP and INIT to the APs go through
  `send_ipi` with 32-bit destinations. `kexec_quiesce` uses `apic::read/write`
  (LVTs, ISR 0x810-0x817), so it works in both modes. It leaves the APIC on
  and in its current mode. The next kernel sees EXTD=1 and stays in x2APIC.
  INIT keeps the mode, so the APs come back in x2APIC as well.
- The kernel does not use Limine's MP request, so Limine never changes the
  APIC mode.

What was missing was a way to reach x2APIC: with fewer than 255 CPUs the
mode was never chosen, even though the CPU supports it (`-cpu max` under TCG
and `-cpu host` both report x2apic).

## Changes
- `apic.x2apic=1` cmdline knob (`apic::set_force_x2apic`, set in `kernel_main`
  before arch init). It switches to x2APIC on any CPU that supports x2APIC.
- `apic::init` now logs the mode on every boot:
  `[CPUS] x2APIC mode (already enabled at entry: firmware or kexec | APIC IDs
  above 254 | apic.x2apic=1)` or `[CPUS] xAPIC mode (x2APIC available, not
  needed | apic.x2apic=1 ignored: ... | )`.
- `[KEXEC] BSP cpuN: INIT sent to N CPU(s) (x2APIC|xAPIC)` names the mode at
  the hand-off.

## Test matrix (all release builds, lane kernels)
Each run was Limine gen0, then gen1 `kexec -e`, gen2 `kexec -e -c 2`, gen3
`kexec -e -c 3` (KVM: `-c 5`), loading kernel-direct from a raw NVMe disk. Every
gen reached a root login, showed nproc = all CPUs, and logged
`[SMP] N CPUs online (<mode>)`.

| host / accel | topology | gen0 mode | gen1-3 | result |
|---|---|---|---|---|
| Mac, TCG | q35 4 CPUs, `apic.x2apic=1` | x2APIC (apic.x2apic=1) | x2APIC (already enabled at entry) | 3/3 |
| Mac, TCG | `-smp 2,sockets=2,cores=256,maxcpus=512` + 2 `-device max-x86_64-cpu` at socket 1 + `intel-iommu,intremap=on,eim=on`: APIC IDs 0,1,256,257 | x2APIC (APIC IDs above 254) | x2APIC (already enabled at entry), cpu:apic 2:256 3:257; gen2/3 sent from IDs 256/257 (NMI to BSP) | 3/3 |
| Mac, TCG | q35 4 CPUs, no knob (regression) | xAPIC | xAPIC | 3/3 |
| desktop, KVM `-cpu host` | `-smp 8,cores=4,threads=2`, `apic.x2apic=1`, 6 GiB | x2APIC | x2APIC, 8 CPUs online | 3/3 |
| desktop, KVM | same, no knob (regression) | xAPIC | xAPIC, 8 CPUs | 2/2 (gen1 BSP, gen2 `-c 3`) |
| Mac, TCG aarch64 (lane kernel in ESP, gen1 = image's /boot/kexec; no aarch64 code changed) | virt 4 CPUs | 4 CPUs | gen1: login, 4 CPUs, cmdline `kexec.gen=1` | 1/1 |

Note: with APIC IDs above 254, OVMF still handed the BSP over in xAPIC mode.
The kernel switched to x2APIC itself. The "firmware left x2APIC on" entry
was exercised only as a kexec'd kernel, which arrives with EXTD=1. That is the
same code path.

Desktop method: no git refresh. The lane kernels were copied by scp to
`/run/media/forain/samsung970pro512/kx2a`, and images were copied from the
desktop's b315ee91 build. That scratch dir was deleted after the runs.

## Out of scope / left
- Interrupt remapping is not implemented. IOAPIC and MSI destinations are
  8-bit, which is fine while device IRQs target the BSP (ID 0). A BSP with an
  APIC ID above 255 would need IR.
- The laptop was not run in x2APIC. Its firmware leaves xAPIC, and
  `apic.x2apic=1` on its kernel cmdline would exercise x2APIC there.
