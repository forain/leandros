# lane/kexechvf (2026-10-19)

Follow-up to `lane-kexecexec-2026-10-19.md`: under HVF the kexec'd kernel
came up on 1 CPU (`[SMP] mpidr=0x1 accepted CPU_ON but never came online`).

## Root cause: QEMU HVF loses CPU_ON for a vCPU that ran CPU_OFF
This is a QEMU bug, checked in the 11.1.1 source (`~/src/macqemu/qemu-11.1.1`).
Upstream master's `arm_set_cpu_on_async_work` is unchanged.

1. The AP's `hvc` CPU_OFF exits to `hvf_handle_vmexit`, case `EC_AA64_HVC`.
   `cpu_synchronize_state` copies HVF's registers into env
   (`vcpu_dirty = true`). Then `hvf_psci_cpu_off` → `arm_set_cpu_off`
   queues the work that sets PSCI_OFF and halts the vCPU, and sets
   `env->xregs[0] = ret` (0).
2. At the end of the exit, `flush_cpu_state` writes env back to HVF and
   **clears `vcpu_dirty`**. HVF now holds PC = the instruction after `hvc`
   and x0 = 0.
3. The vCPU halts (`power_state = PSCI_OFF`), and AFFINITY_INFO reports it
   OFF. That is correct so far.
4. The new kernel calls CPU_ON. `arm_set_cpu_on` → `arm_set_cpu_on_async_work`
   runs on the target thread: `cpu_reset`, `arm_emulate_firmware_reset`,
   `xregs[0] = context`, `cpu_set_pc(entry)`, PSCI_ON. All of this writes
   **env only**. Nothing sets `vcpu_dirty`: there is no
   `cpu_synchronize_post_reset`, unlike a system reset. So
   `hvf_arch_vcpu_exec`'s `flush_cpu_state` pushes nothing.
5. The vCPU resumes from its stale HVF state, after the `hvc` with x0 = 0.
   This matches the previous lane's gdbstub capture exactly: PC was at the
   purgatory's post-hvc `wfi` loop with x0 = 0. It also explains that lane's
   note that CPU_ON "does not reset SCTLR_EL1/VBAR_EL1".

At first boot the secondaries never ran, and `hvf_init_vcpu` /
post-reset left them dirty, so CPU_ON works. TCG has no separate register
copy. KVM handles PSCI in the host kernel. Only HVF re-onlining is affected,
so Linux CPU hotplug and kexec under QEMU HVF should hit the same bug.

Ruled out:
- CPU_ON returns 0 (SUCCESS), so it is not ALREADY_ON or ON_PENDING.
- The conduit is HVC on both sides.
- The entry point is the physical address of the stub that works at first boot.
- Cache maintenance and GIC state cannot matter: the vCPU never fetches
  the entry.

The proper upstream fix is to mark the vCPU dirty in
`arm_set_cpu_on_async_work`, for example with `cpu_synchronize_post_reset`
or an HVF reset hook. That is not filed yet.

## Guest workaround: the AP mailbox (adopted)
A spec-compliant PSCI never returns from CPU_OFF, so the code after the
`hvc` runs only on buggy HVF, and only once CPU_ON was accepted. That code
now finishes the CPU_ON itself:

- **Park page.** The FDT block (16 KiB, already left out of the next
  kernel's `/memory`) gives its last page to the APs
  (`info::AP_PARK_OFF = 0x3000`). The FDT is limited to the first 12 KiB.
  `prepare` copies the purgatory blob there too. `park_off` enters
  `kexec_purgatory_off` in that copy, not in the purgatory page, which the
  new kernel reuses as RAM.
- **FDT property.** The FDT gets `/chosen/leandros,kexec-ap-mailbox`
  (u64) = park page + 0x800. `boot::device_tree` records it, and
  `kexec_ap_mailbox()` returns it.
- **New kernel.** Before each CPU_ON, `smp_init` (`kexec_mailbox_post`)
  writes `(mpidr | 1<<63, entry, context)` to the mailbox, runs `dc civac`
  and `dsb sy`, then issues CPU_ON and `sev`.
- **AP after CPU_OFF returns.** It reads its MPIDR (Aff mask) and polls the
  mailbox (MMU off, so uncached reads, with `wfe` between reads). When the
  tag matches it sets x0 = context, x1-x3 = 0 and branches to the entry.
  Its state already matches what CPU_ON gives: SCTLR at the reset value,
  VBAR 0, EL1, DAIF masked.

On TCG, KVM and real firmware the post-hvc path is unreachable, and the
mailbox write is a harmless store into a reserved page. A kernel booted
without the property (Limine, firmware) skips it.

## Tests (aarch64, Mac)
Setup: release kernels built from this branch. The ESP was a clone of main's
image with the new `kernel.elf` and an `authorized_keys`. The root image was
a clone of main's, with `/boot/kexec` replaced over ssh.

- **HVF.** Limine boot (4 CPUs) → `kexectest -e` → gen1: `[early] cmdline:
  kexec.gen=1`, `[SMP] 4 CPUs online`, login. Then `kexectest -e` → gen2:
  4 CPUs, login, `/proc/cmdline` = `kexec.gen=2`, `nproc` = 4. The first
  hop was started from cpu1 ("stopping 3 other CPU(s) from cpu1"), so
  cpu0 itself was handed over too.
- **TCG (uefi-tcg).** The same chain: gen1 and gen2 each have 4 CPUs and
  reach login, `nproc` = 4.
- `boot/bootparse-tests`: 11/11 pass (`--target aarch64-apple-darwin`).
- x86_64 is not touched: every changed path is aarch64-only. The
  `FdtInput` field is the only shared change.

## Open
- Report the QEMU bug upstream, with the patch: set `vcpu_dirty` after the
  CPU_ON reset.
- If CPU hotplug (offline/online) is ever added, it needs the same treatment
  under HVF.
