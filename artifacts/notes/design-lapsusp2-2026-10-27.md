# design lapsusp2 2026-10-27: real S3 (suspend to RAM) with resume, x86_64 / HP EliteBook 840 G5

Status: DESIGN ONLY. Builds on lane/lapsusp (fb3ffc02: freeze-only device cycle) and lane/lappower
(997cae88: AML interpreter + EC). Correction to lane-lapsusp-2026-10-27.md item 2: an AML interpreter
**does exist** (`drivers/src/acpi_power/aml.rs`). S3 uses it; it does not port a new one.

aarch64 has no ACPI S3. Everything below is `#[cfg(target_arch = "x86_64")]`. On aarch64,
`/sys/power/state` reads as an empty line, and a write returns EINVAL.

---------------------------------------------------------------------------------------------------
## 0. What exists today (anchors)

| What | Where |
|---|---|
| Freeze-only cycle, hook table, reboot(2) `PMTS` | kernel/src/pm.rs:71-80 (DEVS), :96 `freeze_cycle`, :133 `sys_pm_test`; kernel/src/power.rs:40, :127 |
| Shutdown prep we can reuse: `sync_all`, f2fs commit, NVMe CC.SHN | kernel/src/power.rs:144-158; kernel/src/syscall.rs:4531; servers/f2fs/src/lib.rs:9399; drivers/src/nvme.rs:1238 |
| Sleep facts: `_S3_` SLP_TYP, FACS addr, has `_PTS`/`_WAK`, PM1x_CNT ports | boot/src/acpi.rs:157-192 (`AcpiPower`), :238 `find_power_info`, :284 (`_S3_` byte scan) |
| AML interpreter: Host trait, eval, regions | drivers/src/acpi_power/aml.rs:33-45 (Host), :382 `eval`, :392 `eval_child`, :801 `region_addr`, :818/:837 `unit_read/unit_write` |
| Interpreter host (ports, memory, PCI, EC), FADT PM1a_EVT/GPE0 parse, `_INI` order | drivers/src/acpi_power/power.rs:105/:138 (KHost read/write), :171 (`sleep_ms` capped at 20 ms busy-wait), :205-295 (`init`), :326 `with`, :475 `poll` |
| kacpid poll thread (200 ms) | kernel/src/lappower.rs:25 `task`, :41 `init` |
| AP trampoline (0x7000, 16→32→64), param block 0x7F00 | arch/x86_64/src/smp.rs:23-33 (layout), :149-156, :193-283 (asm), :226 (`mov eax,[0x7F00]; mov cr3,eax` = **32-bit CR3**), :416 `sched_ap_entry`, :450 `smp_init`, :494 |
| Per-CPU init pieces reusable on resume | arch/x86_64/src/paging.rs:254 `init_pat_ap`; arch/x86_64/src/gdt.rs:67 (TSS type 9 = available, so re-running it clears BUSY), :184 `init_cpu`; arch/x86_64/src/apic.rs:189 `init`, :265 `kexec_quiesce`; arch/x86_64/src/ioapic.rs:24 `init`, :116 `mask_all` |
| Clock: TSC-epoch monotonic, raw-TSC state | arch/x86_64/src/timer.rs:42-53 (`TSC_PER_TICK`, `EPOCH_TSC`, `GRID_TSC`), :80 `monotonic_ns`, :401 `init_local_timer`, :415/:434/:461 (`ONESHOT_TSC`, `LAST_IRQ_TSC`, `LAST_TICK_TSC`), :582 `on_tick`; arch/x86_64/src/cpufreq.rs (rdtsc) |
| RTC (CMOS) read only | arch/x86_64/src/rtc.rs:34 `cmos_read`, :72 `epoch_secs`; kernel/src/main.rs:2007 `time_init` (`sched::set_realtime_ns`) |
| Model for "one CPU left, temp CR3 with upper half copied" | kernel/src/kexec/exec_x86_64.rs:180-245 (`final_bsp`) |
| Allocation below 4 GiB | mm/src/buddy.rs:623 `alloc_below` |
| Idle halt, the AP park point | sched/src/lib.rs:6940 `ap_entry`, :7250 `scheduler_run_loop`, :7596 `sti; hlt; cli` |
| Persistent klog (raw partition at the end of the stick, not the ESP) | kernel/src/klog.rs:1-40, :456 `flush_now`, :464 `flush_final` |
| Device hooks | arch/x86_64/src/keyboard.rs:458/:493; drivers/src/i915/mod.rs:286-287; drivers/src/e1000e.rs:1694 (init), :2288/:2305; drivers/src/blkdev.rs:625/:649; drivers/src/usb_hcd.rs:674/:699; drivers/usb/src/xhci/mod.rs:175 `Xhci::init` (:254-261 scratchpad, :276 DCBAAP, :277 CRCR, :281-283 ERST), :1260 `probe`; drivers/src/nvme.rs:1184 (init); drivers/src/iwlwifi.rs:1297 (init) |
| login1 / init broker | ports/sysbus/src/login1.rs:73 `initctl_request`, :327 `suspend` (returns NotSupported today); userland/init/src/main.rs:118-188 (`/run/user/initctl`) |
| sysfs overlay pattern | servers/vfs/src/sysclass.rs:42 `set_sysclass_ops` |
| QEMU x86: q35 + OVMF, USB-stick root on qemu-xhci | scripts/run-qemu.sh:170, :307, :820-824 (`--usb-stick`) |

No HPET driver exists (HPET only matters for the PIT/TSC calibration fallback). Nothing about it to restore.

---------------------------------------------------------------------------------------------------
## 1. The flow (Linux `suspend_devices_and_enter` → `acpi_suspend_enter` → `wakeup_64.S`, mapped to us)

New entry: `pm::suspend_mem(opts) -> Result<SleepReport, Errno>`. It is reached from a write of `mem`
to `/sys/power/state`, from reboot(2) `PMTS` with a new flag, or from the boot knob `pm.s3_selftest`.
It runs in the writer's task context, on whichever CPU that task is on, and migrates to CPU 0 first:
the BSP must do the sleep, because firmware resumes only the BSP.

```
SUSPEND                                                     Linux equivalent
 S0  refuse unless pm.s3=1, \_S3_ found, typ_a != \_S5_ typ_a,  (valid_state)
     FACS mapped+signature ok, RTC alarm armed (pm.s3_require_rtc)
 S1  sync_all(); f2fs checkpoint (NOT clean-unmount)            ksys_sync_helper
 S2  klog::flush_final()   -- the last pre-sleep text on the stick
 S3  freeze user tasks                                          freeze_processes
 S4  \_TTS(3) if present                                        acpi_pm_begin → acpi_sleep_tts_switch
 S5  device suspend, MEM semantics, DEVS order (+nvme, hda,     dpm_suspend_start / _late
     iwl, pci-save last)
 S6  kacpid paused; FACS.firmware_waking_vector = WAKE_PHYS;    acpi_pm_prepare → acpi_sleep_prepare
     X_firmware_waking_vector = 0 (FACS ver>=1); wake devices   (acpi_set_waking_vector,
     (none in phase 1); \_PTS(3); \_SI._SST(3)                   acpi_enable_wakeup_devices, _PTS, _SST)
 S7  park APs (each: wbinvd, save ctx, cli;hlt), then INIT them  pm_sleep_disable_secondary_cpus
 S8  cli; save sysdev: IOAPIC RTEs, LAPIC, PM1_EN, GPE0_EN,     syscore_suspend
     TSC/monotonic snapshot, MTRR/PAT/MISC_ENABLE snapshot
 S9  kernel-text CRC + canaries (pm.s3_verify)
 S10 pm_s3_lowlevel(): push callee-saved, save CPU state,       do_suspend_lowlevel (wakeup_64.S)
     call acpi_enter_s3()  ---- never returns on success ----   → acpi_hw_legacy_sleep
RESUME (firmware → WAKE_PHYS, real mode)
 R0  16-bit: cli, cld, segs, [beep], MISC_ENABLE restore,       wakeup_asm.S
     lgdt, PE → 32-bit: CR4.PAE, CR3=temp<4GiB, EFER LME|NXE,
     PG → 64-bit → jmp pm_s3_resume64 (higher half)              wakeup_long64
 R1  restore CR4/CR3/CR0, XCR0, GDT, TSS(clear busy)+ltr, IDT,   restore_processor_state
     segs, EFER/STAR/LSTAR/FMASK/FS/GS/KGS/PAT/TSC_AUX/
     SPEC_CTRL, rsp; return 1 from pm_s3_lowlevel
 R2  ACPI wake prep: SLP_TYP=\_S0_, SLP_EN=0; SCI_EN=1;          acpi_leave_sleep_state_prep
     \_SI._SST(2)                                               + acpi_hw_legacy_wake_prep
 R3  sysdev resume: PIC mask, IOAPIC id+RTEs, LAPIC via          syscore_resume (i8259, ioapic,
     apic::init + init_local_timer, TSC rebase, realtime from    lapic, timekeeping)
     RTC; S9 CRC check (fail → halt, no disk writes)
 R4  sti; INIT/SIPI APs into pm resume entry                    enable nonboot cpus
 R5  clear PM1_STS/GPE0_STS, restore runtime GPE_EN, \_WAK(3),   acpi_pm_finish
     clear WAK_STS, PWRBTN_EN, \_SST(1); waking vector = 0      (acpi_leave_sleep_state)
 R6  device resume, reverse order, pci-restore FIRST;            dpm_resume_noirq/early/resume
     klog flush right after xhci+block are back
 R7  \_TTS(0); kacpid resumed; thaw tasks; lid re-read           acpi_pm_end, thaw_processes
```

If any step from S4 on fails, everything already done is undone in reverse order. `_WAK(3)` always
runs once `_PTS(3)` has run, as Linux's `acpi_pm_finish` does on the error path. The suspend then
returns EIO with a report.

### 1.1 Exact ACPI register sequence (`acpi_enter_s3`, mirrors ACPICA `acpi_hw_legacy_sleep`)

Fixed hardware (ports come from the FADT; on Intel PCH ACPIBASE is usually 0x1800):
`PM1a_EVT` = STS at +0 (16 bit), EN at +PM1_EVT_LEN/2; `PM1a_CNT`; `GPE0_BLK` = STS bytes [0,len/2),
EN bytes [len/2,len).

```
PM1_STS bits: TMR 0, BM 4, GBL 5, PWRBTN 8, SLPBTN 9, RTC 10, PCIEXP_WAKE 14, WAK 15 (all W1C)
PM1_EN  bits: TMR_EN 0, GBL_EN 5, PWRBTN_EN 8, SLPBTN_EN 9, RTC_EN 10, PCIEXP_WAKE_DIS 14
PM1_CNT bits: SCI_EN 0, BM_RLD 1, GBL_RLS 2, SLP_TYP 10..12, SLP_EN 13
```

1. `outw(PM1a_STS, 1<<15)` (and PM1b): clear WAK_STS.
2. Clear all fixed status: `outw(PM1a_STS, 0x8731 | 1<<14)` (ACPI_BITMASK_ALL_FIXED_STATUS + PCIEXP_WAKE).
   Clear all GPE status: `outb(GPE0_STS+i, 0xFF)` for i < len/2.
3. Save, then program enables. GPE: save `GPE0_EN[*]`, write 0 everywhere, then set only the wake GPEs
   (phase 1: none). PM1_EN: save it, then write `RTC_EN | PWRBTN_EN` (PCIEXP_WAKE_DIS=1 in phase 1).
   PWRBTN wakes an Intel PCH from S3 even without this bit; it is the manual fallback.
4. `v = inw(PM1a_CNT) & !(7<<10 | 1<<13)`; `outw(PM1a_CNT, v | typ_a<<10)` (write #1, no SLP_EN; same for
   PM1b with typ_b). Read-modify-write: **never clear SCI_EN**.
5. `wbinvd` (ACPI_FLUSH_CPU_CACHE).
6. `outw(PM1a_CNT, v | typ_a<<10 | 1<<13)`, then PM1b (write #2). The platform enters S3 here.
7. If the CPU is still running: spin up to 2 s for WAK_STS. A timeout means S3 failed; report
   `S3 entry did not happen` and run the full resume path (SLP_TYP back to `\_S0_`, `_WAK`, devices).

Sanity before step 4, refused with EINVAL and logged: `\_S3_` is found; `typ_a < 8`;
`typ_a != _S5_ typ_a` (writing the S5 value powers the machine off). On Intel PCH S3 is `typ=5`, so any
other value is logged as a warning. In QEMU it is 1.

Wake prep (`acpi_hw_legacy_wake_prep` + Linux `acpi_suspend_enter` tail), first thing after R1:
`v = inw(PM1a_CNT) & !(7<<10|1<<13) | s0_typ_a<<10`, then write it. Then `if v & SCI_EN == 0 { outw(v|1) }`;
Linux does this "for bug compatibility". Then `_SST(2)`.

Wake (`acpi_hw_legacy_wake`): `_SST(2)` → clear all PM1/GPE status → restore saved GPE0_EN and PM1_EN
(runtime set) → `_WAK(3)` → `outw(PM1a_STS, 1<<15)` → PWRBTN_EN=1, clear PWRBTN_STS → `_SST(1)`.
Read `PM1_STS` before clearing it, and log the wake source: RTC 10 / PWRBTN 8 / WAK 15 / GPE n.

### 1.2 FACS

The address is `AcpiPower.facs` (verify that boot/src/acpi.rs prefers X_FIRMWARE_CTRL at FADT+132 over
FIRMWARE_CTRL at +36). FACS is in ACPI NVS. Map it explicitly (the HHDM may not cover NVS); the
`mem_va` pattern is at drivers/src/acpi_power/power.rs:86.

```
+0 "FACS"  +4 length  +8 hardware_signature u32  +12 firmware_waking_vector u32
+16 global_lock u32  +20 flags u32  +24 X_firmware_waking_vector u64 (ver>=1)  +32 version u8
```

- Write `firmware_waking_vector = WAKE_PHYS` (0x7400; the firmware jumps in real mode to CS=0x0740, IP=0).
- If `version >= 1`, write `X_firmware_waking_vector = 0`, so the firmware takes the legacy real-mode
  path (Linux `acpi_set_waking_vector(addr, 0)`). Never use the 64-bit X vector; its entry state is
  firmware-defined.
- Store `hardware_signature` at suspend and compare it on resume. A difference is logged as
  `hardware signature changed`. Linux only warns; we do the same.
- Write 0 back to the vector after resume (in `acpi_pm_finish`).
- Global lock (+16, bit 0 pending, bit 1 owned): AML fields declared `Lock` on HP's EC need it against
  SMM. Implement acquire/release in lane s3aml (§3.1 gap 6).

### 1.3 Wake trampoline and CPU state (lane s3core)

**Placement.** Use the existing AP page at 0x7000 (it is known to be free, and SMP already identity-maps
it), at offset 0x400 (`WAKE_PHYS = 0x7400`). AP code occupies 0x000-0x13F, the GDT 0xE00, and the
parameters 0xF00. The S3 parameter block goes at 0x7D00. Use `.org` guards, as the AP blob does. A
separate 16-bit stack is at 0x7C00 (grows down from 0x7CF0).

**The 32-bit CR3 problem.** smp.rs:226 loads CR3 with a 32-bit move and assumes a page table root
below 4 GiB (:494). On the 16 GB laptop the live PML4 can be above that. At `pm` init, allocate a
**temporary PML4 with `alloc_below(0, 4 GiB)`** (mm/src/buddy.rs:623), plus the PDPT/PD/PT pages that
identity-map 0x7000. Copy entries 256..511 (the kernel half) from the live root at suspend time,
exactly as kexec does (exec_x86_64.rs:226-231). The 64-bit stub then jumps to the higher-half
`pm_s3_resume64`, which loads the real 64-bit CR3. Give AP resume the same temp root; this also fixes
a latent AP-boot bug.

**Real-mode stub** (Linux `wakeup_asm.S`):
```
cli; cld; mov ax,cs; mov ds,ax; mov es,ax; mov ss,ax; mov sp,0x0CF0 (relative to 0x7000 base)
cmp dword [sig], 'LS3W' ; jne die     -- the param block must be ours
[if flags & BEEP] PIT ch2: out 0x43,0xB6; out 0x42,lo; out 0x42,hi (≈880 Hz); in 0x61; or 3; out 0x61; delay; and ~3
[if MISC_ENABLE saved] ecx=0x1A0; eax/edx=saved; wrmsr   -- firmware may set XD_DISABLE (bit 34): NX would #PF
lgdt [gdtr]; cr0.PE=1; ljmp 0x08:pm32
pm32: segs=0x10; cr4 = PAE|PGE?no (PAE|OSFXSR); cr3 = temp_root(<4GiB)
      EFER(0xC0000080) |= LME | NXE (if CPUID NX) ; cr0 |= PG|WP ; ljmp 0x18:pm64
pm64: rsp = saved early stack (phys-identity ok); jmp [resume64_va]  (64-bit absolute)
```

**Saved BSP state** (`#[repr(C)] struct S3Ctx`, a static in kernel .bss; its physical address goes in
the param block):
CR0, CR3, CR4, CR8, XCR0 (if OSXSAVE), GDTR, IDTR, TR selector; EFER, STAR, LSTAR, CSTAR, SFMASK,
FS_BASE, GS_BASE, KERNEL_GS_BASE, IA32_PAT, TSC_AUX, MISC_ENABLE, APIC_BASE (EN/EXTD), SPEC_CTRL (if
present), TSC_ADJUST (0x3B, if present), MTRR default type + fixed/variable (snapshot only); rbx, rbp,
r12-r15, rsp, rflags; an XSAVE/FXSAVE area of the current task's FPU (MXCSR and x87 CW are reset by
firmware).

`pm_s3_lowlevel` is setjmp-like asm. It returns 0 on the way down, after calling `acpi_enter_s3`, which
only returns on failure. It returns 1 when it is entered from `pm_s3_resume64`.

**Restore order** (`__restore_processor_state` + `fix_processor_context`):
MISC_ENABLE (already done in real mode) → CR4 (first OSFXSR/OSXSAVE, then the full value) → XCR0 →
CR3 → CR0 → EFER (|SCE) → `lgdt` → `gdt::init_cpu(0)`, which rewrites the TSS descriptor as type 9 and
so clears BUSY; otherwise `ltr` raises #GP → `ltr` → `lidt` → reload DS/ES/SS = 0, CS via far return
→ FS/GS/KGS bases → STAR/LSTAR/CSTAR/SFMASK (`syscall::init_ap`-equivalent for the BSP) → PAT
(`paging` PAT init) → TSC_AUX = 0 (the `arch_cpu_id` rdtscp fast path) → SPEC_CTRL → FXRSTOR/XRSTOR →
compare MTRRs with the snapshot (log a mismatch; do not rewrite them in phase 1).

**Interrupt controllers.**
- PIC: re-mask both 8259s (0x21/0xA1 = 0xFF); the firmware leaves them live.
- IOAPIC (Linux `ioapic_suspend/resume`): save every RTE (`IOREGSEL` 0x10+2i / 0x11+2i, count from
  `IOAPICVER[23:16]+1`) and the ID register. On resume, write the ID, then every RTE **masked**, then
  the saved values.
- LAPIC: re-run `apic::init()` (it re-enters x2APIC: APIC_BASE EN then EN|EXTD), then
  `timer::init_local_timer()`, then write ESR, then EOI. Rely on re-init over saved LVTs; the boot
  values are what we want.

**TSC / clocks.** The TSC restarts near 0 after S3. CLOCK_MONOTONIC must not jump (Linux excludes
suspend time from it):
- before: `mono = monotonic_ns()`, `rtc0 = rtc::epoch_secs()`.
- after: `EPOCH_TSC = rdtsc() - mono*TSC_PER_TICK/10_000_000`. Rebase every stored raw TSC by the same
  delta: `GRID_TSC`, `ONESHOT_TSC[*]`, `LAST_IRQ_TSC[*]`, `LAST_TICK_TSC[*]` (timer.rs:53/:415/:434/:461),
  plus cpufreq.rs's APERF/MPERF/TSC snapshots. Do all of this **before** the LAPIC timer is re-armed;
  otherwise `on_tick` catch-up adds about 10^9 ticks. Lane s3core greps every `rdtsc` (arch/x86_64 8+15,
  cpufreq 5, smp 3, kernel/main 3, klog 1, sched/random 1, snd 1) and classifies each one.
- realtime: `sched::set_realtime_ns(rtc_now * 1e9)` re-seeds the offset. Report `slept = rtc_now - rtc0`.
- CLOCK_BOOTTIME: if sched has no separate boottime offset, add one, `+= slept` (gap; logind and COSMIC
  idle timers use BOOTTIME). Small; s3core.
- On each AP, before `init_local_timer`: if TSC_ADJUST exists, write the BSP's value into it (Linux
  `tsc_verify_tsc_adjust`). After S3 all cores come out of reset together, so their TSCs agree to
  within a few hundred cycles. Log the max skew measured.

**RTC alarm** (CMOS, Linux `rtc-cmos` + `cmos_set_alarm`; the only CMOS registers ever written):
wait for !UIP (reg 0x0A bit 7); read reg 0x0B for DM (bit 2, binary vs BCD) and 24h (bit 1); read reg
0x0C (clears AF); write the alarm into 0x01 (sec), 0x03 (min), 0x05 (hour), now+N with rollover via
epoch math. If FADT `DAY_ALRM` (byte 113) == 0x0D, write reg 0x0D bits 5:0 = 0 ("don't care", Intel
PCH), preserving bit 7. If `MON_ALRM` (byte 114) != 0, log it and leave it alone. Set reg 0x0B AIE
(bit 5). After resume: clear AIE, read reg 0x0C. **Never write 0x0E..0x7F, or bank 1 (0x72/0x73):
those are HP's checksummed NVRAM.**

### 1.4 APs (lane s3cpus)

The sched has no CPU hotplug. Do not build one. Park the APs in place:

- Park request: set `PM_PARK = true`, then send a resched IPI to every AP. In `scheduler_run_loop`
  right before the idle `sti; hlt; cli` (sched/src/lib.rs:7596), an AP that sees `PM_PARK` calls
  `arch_pm_ap_park(cpu)`. Asm saves rbx/rbp/r12-r15/rsp/return-rip into `AP_CTX[cpu]`, does `wbinvd`,
  sets `PARKED[cpu]`, then loops on `cli; hlt`.
- An AP parks only from its idle path. The freezer (below) guarantees it gets there. The BSP waits up
  to 2 s for all of them. A timeout aborts the suspend (unpark: clear `PM_PARK`, NMI kick; the AP
  returns from park via the same longjmp).
- After all are parked, the BSP sends INIT (assert) to each, as `final_bsp` does
  (kexec/exec_x86_64.rs:186-195). After S3 they are in wait-for-SIPI anyway.
- Resume: the BSP reuses the `smp_init` INIT, SIPI, SIPI sequence with the AP trampoline. The param
  block gets `entry = sched_ap_resume_entry` and the temp root. `sched_ap_resume_entry` = the
  `sched_ap_entry` body (smp.rs:416-438: `init_pat_ap`, `seed_cpu_id_fastpath`, `apic::init`,
  `enable_sse`, `gdt::init_cpu` (clears TSS busy), `idt::load`, `syscall::init_ap`, TSC_ADJUST sync,
  `init_local_timer`, `cpufreq::init_cpu`), **without** the `ACTIVE_CPUS` increment. It then longjmps
  to `AP_CTX[cpu]`, so the AP returns from `arch_pm_ap_park` into its idle loop.
- Freezer (same lane): `PM_FREEZE` is checked at return-to-user and in the signal-delivery path. A user
  thread that sees it blocks (TASK_INTERRUPTIBLE, not visible as a stop) until thaw. The suspending
  task is exempt. Kernel threads run on (the device hooks need them) and are parked implicitly when
  their CPU parks. Their runqueue entries stay queued until resume.

### 1.5 Kernel self-check (pm.s3_verify, default on whenever S3 is used)

Before S10: CRC32 of the kernel `.text` + `.rodata`, plus 64 canary pages (allocated, filled with a
pattern, and one in the first 1 MiB if buddy owns any low pages). After R3: recompute. A mismatch
prints `[S3] VERIFY FAILED ...` and halts **with the block gate still closed**. No byte reaches the
stick from a kernel whose memory the firmware scribbled on. The user holds power, and the next boot is
clean. Also check that buddy never hands out <1 MiB pages other than 0x7000 (Linux reserves low
memory against BIOS scribbling).

---------------------------------------------------------------------------------------------------
## 2. Device restore after power loss (MEM semantics)

The current hooks are freeze-only (state is kept). Every driver gets `pm_suspend_mem` and
`pm_resume_mem`. `DEVS` (pm.rs:71) gains a `mem` column, and new entries in this order: i8042 →
i915 → hda → e1000e → iwlwifi → block → nvme → xhci → **pci (save last / restore first)**.
Resume runs in reverse: pci → xhci → (klog flush) → nvme → block thaw → iwl → e1000e → hda → i915 →
i8042.

**pci (lane s3pci).** Linux `pci_save_state` / `pci_restore_state`. Run it over every function,
bridges first on restore (bus order):
- config dwords 0x00-0x3F
- MSI: ctrl, addr lo/hi, data, mask
- **MSI-X**: ctrl + every table entry, read back from BAR MMIO, because the table lives in device
  memory and is lost
- PCIe cap: DevCtl, LnkCtl, SltCtl, RtCtl, DevCtl2, LnkCtl2
- LTR ext cap; L1SS ctl1/ctl2
- PM cap: PMCSR

Restore: PMCSR → D0 (+10 ms if it was D3hot), write 0x3C..0x10 with COMMAND=0, then COMMAND last,
then the caps. A function whose VID reads 0xFFFF after resume is logged (`link down behind root port
xx`), and its driver's resume reports failure. Phase 1 does not put devices into D3 before sleep: the
firmware powers them off.

**xHCI (lane s3xhci). The root file system is on the USB stick; this is the critical one.**
Linux `xhci_suspend` / `xhci_resume`.

Suspend:
1. Stop the root-hub/port poll.
2. Clear the PORTSC wake bits WCE/WDE/WOE (no USB wake in phase 1).
3. `USBCMD.R/S=0`, wait HCH ≤ 16 ms.
4. Save the op and interrupter-0 registers: USBCMD, DNCTRL, DCBAAP, CONFIG, IMAN, IMOD, ERSTSZ,
   ERSTBA, ERDP. CRCR reads 0, so keep our own command-ring dequeue/cycle.
5. `USBCMD.CSS=1`, wait `USBSTS.SSS=0` ≤ 20 ms. If `USBSTS.SRE`: log it and mark "will reinit".

Resume:
- Restore path, when `!SRE && !HCE && !pm.s3_xhci=reinit`: write back DNCTRL, DCBAAP, CONFIG, ERSTSZ,
  ERSTBA, ERDP, IMOD, IMAN. Clear the command ring (zero it, enqueue = dequeue = start, cycle = 1),
  then `CRCR = ring | RCS`. Set `USBCMD.CRS=1`, wait `RSS=0` ≤ 10 ms. If `USBSTS.SRE` is now set, take
  the reinit path. Otherwise `USBCMD |= R/S|INTE`, wait !HCH. Then for each root port: if CCS and
  PLS=U3, write PLS=U0 with LWS; if CSC is set or the link is gone, take the reset-resume path below
  for that port.
- Reinit path ("xHC lost power", the common case on laptops, and **always** in QEMU, which resets
  every device on wake): HCRST, `Xhci::init`-equivalent on the same controller object (new or zeroed
  DCBAA, rings, ERST, and the same scratchpad pages), R/S. Then for each previously attached device,
  **USB persist** (Linux `usb_reset_and_verify_device`): port reset → Enable Slot → Address Device →
  read the device + config descriptors and the serial string, and compare them with the saved copy.
  On a match, call the class driver's `reset_resume(new slot, endpoints)`, which swaps the transport
  of the **existing** usb_storage LUN / usb_hid instance in place. The blkdev minor, the f2fs mount and
  the evdev node stay. On a mismatch, remove the device. If that device holds the root, log
  `[S3] root device changed across suspend: halting, no writes` and halt.
- usb_storage after reattach: TEST UNIT READY. Expect UNIT ATTENTION 06/29/00 (power-on reset) and
  retry up to 5×. Then READ CAPACITY(10/16) must equal the saved value. Only after that open the
  block gate.

Intel SPT-LP xHCI (8086:9d2f) carries Linux's `XHCI_PME_STUCK_QUIRK` (vendor reg 0x80A4 bit 28 on
suspend). It only matters for wake, so phase 2.

**NVMe (lane s3nvme).** Suspend: the block gate is already drained; reuse `nvme::shutdown_all`
(nvme.rs:1238: delete the I/O queues, CC.SHN=01, wait CSTS.SHST=10) and keep the queue memory.

Resume (Linux `nvme_reset_ctrl`), per controller:
1. CC.EN=0, wait RDY=0.
2. AQA/ASQ/ACQ with the same, zeroed admin queues; reset head, tail and phase.
3. CC (IOSQES=6, IOCQES=4, MPS, CSS) | EN, wait RDY=1 ≤ CAP.TO×500 ms.
4. Set Features #Queues; recreate the I/O CQs/SQs on the same zeroed memory.
5. Re-enable the **Host Memory Buffer** (Set Features 0x0D, the same descriptor list) if init used
   one, and APST if init set it.

Refactor nvme.rs:1184 `init` into a per-controller `bring_up(&Ctrl)` used by both paths.

**e1000e (I219-LM, lane s3nvme).** Linux `__e1000_shutdown` / `e1000e_pm_resume`. Suspend: the
existing down path, WUFC=0, CTRL_EXT.DRV_LOAD=0. Resume: **disable ULP first** (`e1000_disable_ulp_lpt_lp`:
I219 can come back in ULP with the PHY inaccessible; toggle LANPHYPC), then the init path's reset/PHY
init on the existing netdev, rings re-zeroed, RCTL/TCTL, IMS, autoneg. The netstack sees link
down→up and the DHCP lease survives.

**i915 (lane s3i915, the largest).** Linux `i915_drm_suspend` / `i915_drm_resume`.

Suspend:
- display commit "all off", saving the committed state: pipe/transcoder/DPLL/eDP PPS order as on
  disable
- GEM idle + flush
- RC6/RPS off
- IRQs off

Resume:
- forcewake
- **re-apply workarounds** (wa.rs), MOCS
- **GGTT: rewrite every PTE** (stolen GSM is DRAM and was retained, but Linux rewrites; do the same)
- per engine: RING_MODE, HWS_PGA, execlists enable
- RC6/RPS (rps.rs)
- DMC firmware reload if init loaded one
- display: cdclk → DPLL → eDP panel power on with the VBT T1-T3 delays → link training → pipe/planes
  → backlight PWM, through kms's own modeset path with the saved state
- hotplug/vblank IRQs

Phase 1 (first hardware tests) accepts a dark panel: `pm.s3_i915=skip` leaves display and GT off after
resume, and the test is judged by klog. The HDA HDMI codec needs the display power well, so i915
resumes before hda.

**iwlwifi (Intel 8265, lane s3iwlhda).** No WoWLAN. Suspend: deauth, stop the device (APMG stop, SW
reset). Resume: the full firmware load path of `init` (iwlwifi.rs:1297; FH DMA sections, ALIVE, NVM,
PHY_CFG) on the existing netdev, then carrier off→on. NetworkManager and the supplicant reassociate.
rfkill state is re-read.

**HDA (lane s3iwlhda).** Linux `azx_resume` + `snd_hda_codec_resume`. Controller: CRST 0→1, CORB/RIRB
on the same memory (CORBRP reset handshake, RIRBWP reset, RINTCNT), INTCTL. Codec: AFG to D0, then
**replay the cached verbs** (pin ctls, amps, EAPD, connection selects, unsol jack enables). Streams:
SDnCTL SRST, then reprogram on the next prepare. pipewire_server sees an xrun and re-prepares.

**i8042** (in s3core): self-test 0xAA → 0x55, restore CTR, keyboard reset 0xFF, LEDs. Touchpad
re-init is psmouse's or lapi2c's job (DesignWare I2C/LPSS: deassert reset at LPSS priv 0x204,
reprogram the SCL timings). Listed for lapi2c; not phase 1.

**Others, later.** USB-C/Thunderbolt (lapusbc), UVC camera (re-enumerated by xHCI persist), Bluetooth
(USB, same). TPM: we send no TPM2_Shutdown today, at power-off either, so S3 changes nothing.

---------------------------------------------------------------------------------------------------
## 3. AML: `_TTS` / `_PTS` / `_SST` / `_WAK` / `_S0_` with the existing interpreter (lane s3aml)

API (in `acpi_power`, built on lapi2c's `with_ns`):
```rust
pub fn sleep_prepare(state: u8) -> SleepAml   // _TTS(3), _PTS(3), \_SI._SST(3); logs every OpRegion write
pub fn sleep_wake(state: u8) -> SleepAml      // \_SI._SST(2), _WAK(3) → (status, pss), _SST(1)
pub fn tts(state: u8)                         // _TTS(0) at the very end
pub fn sx_typ(name: &str) -> Option<(u8,u8)>  // evaluate \_S0_ / \_S3_ / \_S5_ packages; cross-checks boot's byte scan
pub fn pause_poll(on: bool)                   // kacpid stops evaluating (EC queries / GPE methods) during the window
```
`_WAK` returns `Package{status, PSS}`. Log it: bit 0 = wake signaled but failed for lack of power;
bit 1 = thermal. Notify()s queued during `_WAK` (HP commonly does `Notify(LID0,0x80)`,
`Notify(BAT0,0x81)`, `Notify(ADP1,0x80)`) are handled by the normal `handle_notes` after thaw, so lid,
battery and AC are re-read.

### 3.1 Interpreter gaps that matter for S3

1. **Region spaces > 3 are silently dropped.** `unit_read/unit_write` (aml.rs:818-855) only reach the
   host for spaces 0-3. That drops SystemCMOS (5; HP stashes sleep flags in CMOS fields), SMBus (4),
   GPIO (8), GenericSerialBus (9) and PCC (10). Add a per-space "dropped access" counter and log it in
   `[S3] aml:` lines. SystemCMOS gets **read** support (0x70/0x71 and 0x72/0x73). Writes stay dropped
   unless `pm.aml_cmos_write=1`, because a wrong-offset write there is the one way AML could change
   BIOS settings.
2. **`Sleep` is capped at 20 ms of busy-wait** (power.rs:171). EC handshakes in `_PTS` use Sleep(100+).
   Inside sleep methods, honour up to 2000 ms with a real `msleep`. `Stall` (≤100 µs) is fine.
3. **`Acquire` always succeeds** (aml.rs:1308). That is fine once kacpid is paused (gap 4); the lane
   asserts it.
4. **Concurrency:** kacpid (lappower.rs:25) evaluates `_Qxx` and GPE methods every 200 ms under the same
   STATE mutex. `pause_poll(true)` before `_TTS`, `false` after `_TTS(0)`. EC SCI_EVT stays pending
   meanwhile and is drained after.
5. `Load` / `LoadTable` / `DataTableRegion` are unsupported (aml.rs:593, :1318). Not used by sleep paths
   on Kaby Lake HP firmware (they load CPU SSDTs from `_PDC`/`_OSC`). Log if hit.
6. **Global lock not implemented.** `Field(..., Lock, ...)` on the EC region should take the FACS
   global lock against SMM. Implement acquire (`cmpxchg` on FACS+16: set owned, or pending if owned)
   and release (if pending: write GBL_RLS, PM1_CNT bit 2). Take it around every access to a field
   flagged Lock. This is cheap, and S3 maps the FACS anyway.
7. **Opcode coverage:** `_PTS`/`_WAK` on HP touch EC fields (lapdsdt is fixing EC reads; **s3aml
   depends on that merge**), SMI_CMD (SystemIO 0xB2, supported), GPIO-pad SystemMemory (supported) and
   `\_SB.PCI0.LPCB.*` methods. The lane runs `_PTS`/`_WAK` with `pm.amltrace=1` on the laptop's dumped
   DSDT (offline test harness over the real tables, `acpidump`-style blob from the `[ACPI]` dump) before
   any hardware run, and lists every BadOpcode/Undefined.
8. `\_SB` methods: none are mandatory for RTC-only S3. Phase 2 (wake sources): `_PRW` (GPE + deepest
   Sx) for LID0/PWRB, then `_DSW(1, 3, 0)` (or legacy `_PSW(1)`) before `_PTS` and `_DSW(0,0,0)` after
   `_WAK`. `\_SB._SWS` / `\_GPE._SWS` are optional, wake-source logging only.

---------------------------------------------------------------------------------------------------
## 4. Userspace: `/sys/power`, rtc wakealarm, login1, lid (lanes s3sysfs, s3policy)

The in-kernel VFS overlay follows the `sysclass` pattern (servers/vfs/src/sysclass.rs:42). The kernel
registers ops from `pm`.

| File | Read | Write |
|---|---|---|
| `/sys/power/state` | `mem\n` iff pm.s3=1 and S3 is valid, else `\n` | `mem` → `pm::suspend_mem`; anything else EINVAL |
| `/sys/power/mem_sleep` | `[deep]\n` (or `\n`) | `deep` accepted |
| `/sys/power/pm_test` | `[none] core processors platform devices freezer` | selects the test level (Linux semantics, below) |
| `/sys/power/wakeup_count` | wake-event counter | write back: EBUSY if it changed (systemd-sleep protocol) |
| `/sys/class/rtc/rtc0/wakealarm` | armed epoch or empty | `+N` / epoch / `0` (rtcwake, systemd) |
| `/sys/class/rtc/rtc0/since_epoch` | `rtc::epoch_secs()` | - |

`pm_test` levels (Linux `kernel/power/suspend.c` `suspend_test`) give the staged plan for free:
- `freezer`: freeze and thaw only
- `devices`: + MEM device suspend, then resume, no AML. Today's freeze_cycle is this level with
  freeze-only hooks.
- `platform`: + `_TTS`/`_PTS`/`_SST` … `_WAK`, still awake
- `processors`: + AP park and unpark
- `core`: + sysdev save → `pm_s3_lowlevel` → jump directly to `pm_s3_resume64` through the temp CR3
  (no SLP_EN, no real mode) → sysdev restore
- `none`: real S3

**login1** (ports/sysbus/src/login1.rs):
- `CanSuspend` is `yes` iff `/sys/power/state` contains `mem`, else `na`.
- `Suspend(interactive)`: emit `PrepareForSleep(true)`, wait for delay inhibitors
  (`InhibitDelayMaxSec`=5), call `initctl_request("suspend")` (blocks across the sleep), then emit
  `PrepareForSleep(false)`.
- init (userland/init/src/main.rs:188 `initctl_poll`) gains `suspend` with the same local-session
  authorization as poweroff. Being root, it writes `mem` to `/sys/power/state` and answers `ok` or
  `failed: <errno>`.
- COSMIC (cosmic-osd / settings-daemon) already calls login1.

**Lid policy:** login1 opens the ACPI evdev device (`SW_LID` from acpi_power `set_input_sink`, mod.rs:98).
Settings come from `/etc/leandros/logind.conf` (`HandleLidSwitch`, `HandleLidSwitchExternalPower`,
`HandleLidSwitchDocked`; **all default `ignore`**), and `handle-lid-switch` inhibitors (cosmic-comp
takes one with an external output) are honoured. A lid-close edge with the setting `suspend` and no
block inhibitor calls Suspend.

After resume, ignore lid events for 2 s, then re-read `_LID`. That prevents an immediate re-suspend when
`_WAK`'s Notify reports a stale "closed" (Linux `lid_init_state`). Also refuse suspend when
`pm.s3_require_rtc` is still on (the default during bring-up), with the message
`suspend: bring-up mode, needs an RTC alarm (pm.s3_require_rtc=0)`.

---------------------------------------------------------------------------------------------------
## 5. Knobs (all inert by default)

| Knob | Default | Effect |
|---|---|---|
| `pm.s3=1` | off | Master switch. Without it, nothing below can run: `/sys/power/state` is empty and `suspend_mem` returns EINVAL. |
| `pm.s3_require_rtc=0` | **require** | Refuse `mem` unless an RTC alarm ≤ 300 s is armed. Lifted only after the hardware tests pass. |
| `pm.s3_selftest=N` | off | Kernel thread, 60 s after boot: arm RTC +N s, `suspend_mem`, log, then `pm.s3_selftest_after=reboot\|stay` (default reboot after a 15 s klog flush). The no-ssh test path. |
| `pm.s3_beep=1` | off | PIT beep in the real-mode stub (Linux `acpi_sleep=s3_beep`). |
| `pm.s3_led=1` | off | i8042 0xED LED breadcrumbs after long mode: Caps = CPU restored, Num = sysdev restored, both = devices done. |
| `pm.s3_verify=0` | on | Kernel CRC + canaries. |
| `pm.s3_noaml=1` | off | Skip `_TTS`/`_PTS`/`_WAK` (diagnostic only). |
| `pm.amltrace=1` | off | Log every OpRegion write made by sleep methods. |
| `pm.aml_cmos_write=1` | off | Let AML write SystemCMOS. |
| `pm.s3_xhci=auto\|restore\|reinit` | auto | Choose the xHCI resume path. |
| `pm.s3_i915=skip` | resume | Leave the display dark (phase 1 tests). |
| `pm.s3_flush_each=1` | off | Flush klog after every device resume (slow on the stick; tests only). |
| `pm.trace=1` | off | Linux PM_TRACE: last stage hash in the RTC time registers, reported on next boot. Breaks the wall clock until NTP or hwclock; last resort only. |
| login1 `HandleLidSwitch*` | ignore | Lid policy. |

---------------------------------------------------------------------------------------------------
## 6. Expected log lines (QEMU real S3, `pm.s3=1`, RTC +20)

```
[ACPI] sleep: LOW_POWER_S0=0 \_S3_=1/1 SLP_TYP=... FACS=0x... _PTS=1 _WAK=1        (existing)
[S3] facts: FACS v2 sig 0x........ vec@+12 X-vec cleared; PM1a_EVT 0x600 CNT 0x604 GPE0 0x620/16; S0 0/0 S3 1/1 S5 0/0; wake page 0x7400 temp root 0x........
[S3] suspend: rtc alarm +20 s (07:41:33); verify on; cpus 4
[S3] sync: 3 fs, f2fs checkpoint ok in 41 ms; klog flushed
[S3] freeze: 112 user threads frozen in 3 ms
[S3] aml: _TTS(3) ok; _PTS(3) ok in 2 ms (EC w 3, SystemIO w 1, dropped CMOS 0); _SST absent
[PM] suspend i8042/i915/hda/e1000e/iwl/block/nvme/xhci/pci: ok in ... us: ...   (one line each)
[S3] aps: 3 parked in 1 ms, INIT sent
[S3] sysdev: ioapic 24 RTEs, pm1_en 0x0120 gpe_en 00..; crc 0x........ canaries 64
[S3] entering S3: PM1a_CNT 0x0001 -> 0x0401 -> 0x2401, wbinvd
[S3] resumed: wake PM1_STS 0x8400 (WAK RTC) after ~20 s (rtc), mono held at 123456789 us
[S3] cpu: CR3/GDT/TSS/IDT/MSRs restored; MISC_ENABLE ok; MTRR same; tsc rebased by -0x...
[S3] sysdev: pic masked, ioapic restored, lapic x2apic re-enabled, timer armed; verify ok
[S3] aps: 3 resumed (tsc skew max 210 cycles)
[S3] aml: _SST(2); _WAK(3) -> (0, 0) in 3 ms; waking vector cleared; _SST(1)
[PM] resume pci/xhci(reinit: 1 dev persisted, root LUN same capacity)/nvme/block/... ok
[S3] klog flushed after resume (block online)
[S3] aml: _TTS(0); kacpid resumed; thawed 112 threads; total 2.3 s down / 1.9 s up
```
Failure shapes the lane must produce: `[S3] refused: <reason>`, `[S3] aml: _PTS failed: <err>
(continuing without)`, `[S3] aps: cpu2 did not park; aborting`, `[S3] S3 entry did not happen (no
WAK_STS in 2 s); resuming`, `[S3] VERIFY FAILED: text crc ... != ...; halting, block gate closed`,
`[S3] xhci: restore failed (SRE); reinit`, `[S3] root device changed across suspend: halting`.

---------------------------------------------------------------------------------------------------
## 7. Safe first-test plan

### QEMU first (x86_64 TCG, both arches regress after each lane)

- **Q0 inert:** boot both arches without knobs. `/sys/power/state` is empty and regress is unchanged.
  Required for every lane.
- **Q1:** `--usb-stick` (root on qemu-xhci, the laptop topology) with `pm.s3=1`. Step `pm_test`
  through freezer, devices, platform, processors, core, writing `mem` each time. QEMU resets all
  devices on S3 wake, so the `core` level is the last one possible before the device lanes land.
- **Q2 real S3:** `echo +20 > /sys/class/rtc/rtc0/wakealarm; echo mem > /sys/power/state`. Serial is
  the evidence. The q35 DSDT already has `\_S3_` (lapsusp saw typ 1/1). If OVMF resumes by rebooting
  instead, check that the build has S3 (fw_cfg `etc/system-states`) and add
  `-global ICH9-LPC.disable_s3=0` behind a new `run-qemu.sh --s3` flag. QEMU monitor `system_wakeup`
  is the manual fallback. QEMU has virtio-blk/gpu/net in the default config; the device lanes add a
  `virtio` resume (reset + feature renegotiation + same rings) or the test uses `--usb-stick` with no
  virtio disk. The GPU may stay dark.
- **Q3:** 20 S3 cycles in a loop with suspendtest `--io-during`: no f2fs errors, `bad=0`.

### Laptop (HP 840 G5; no serial). Via the laptop-deploy-kernel skill only

That skill keeps the previous good kernel and the factory kernel as Limine entries, so a hang means
hold power for 4 s and the next boot is the good kernel. Conditions for every run: on AC, lid open, no
external display, no USB devices except the stick, and nothing of value unsynced.

- **H0:** boot with no knobs and record the `[ACPI] sleep:` and `[S3] facts:` lines. Gate: `typ_a=5`,
  `LOW_POWER_S0=0`, the FACS signature is valid, and `_PTS`/`_WAK` load without BadOpcode in
  `pm.amltrace=1` offline run.
- **H1:** `suspendtest` (pm_test=devices, freeze-only). It has never run on the laptop.
- **H2:** pm_test=platform: HP's `_PTS(3)`/`_WAK(3)` run while awake. Afterwards check that fans,
  keyboard, battery `_BST`, the lid and the EC are all normal.
- **H3:** pm_test=processors, then core.
- **H4 real S3, kernel-driven:** `pm.s3=1 pm.s3_selftest=20 pm.s3_beep=1 pm.s3_led=1 pm.s3_i915=skip
  pm.s3_flush_each=1`, one-shot entry. Expect the power LED to blink (S3), a beep about 20 s later,
  the Caps LED, then a reboot about 15 s after that. Evidence: the klog slot on the stick (fetch it
  with laptop-remote-debug after reboot). It holds the pre-sleep text flushed at S2 and the RAM ring
  (which survived S3) flushed as soon as xHCI is back.
  - Diagnosis by symptom: no beep = the firmware never reached our vector (FACS or vector wrong, or
    trampoline). Beep but no Caps = the 16→64 path or the CPU restore. Caps but no reboot or log = the
    sysdev or xHCI resume.
  - The stick's raw klog partition is better than the ESP: writing it needs no FAT code and is already
    proven.
- **H5:** the same driven from ssh (`nohup sh -c 'echo +20 > .../wakealarm; echo mem > /sys/power/state;
  suspendtest --disk-after --net-after' &`). Needs the e1000e or iwl lane to come back.
- **H6:** full display (drop `pm.s3_i915=skip`), then desktop Suspend through COSMIC/login1. Then
  `pm.s3_require_rtc=0` plus lid policy `suspend`, for supervised use.

### Risks that could brick a boot or lose data, and the guards

| Risk | Guard |
|---|---|
| AML or our code writes CMOS NVRAM 0x0E-0x7F / bank 1, corrupting HP BIOS settings (boot order, Secure Boot, so the USB boot is lost) | We write only RTC regs 0x01/0x03/0x05/0x0B/0x0C and 0x0D bits 5:0. AML CMOS writes are dropped unless `pm.aml_cmos_write=1`. `pm.trace` stays off. |
| Wrong SLP_TYP (S5 value) or a clobbered PM1_CNT (SCI_EN cleared) | Validation in §1.1. RMW writes only. The value is logged before the write. |
| Resumed kernel with corrupted memory writes garbage to the root stick | `pm.s3_verify` CRC + canaries **before** the block gate opens. Halt with the gate closed on mismatch. sync + f2fs checkpoint before sleeping, so a forced power-off loses nothing. |
| Root USB stick re-enumerates as "new device", and f2fs gets EIO | USB persist match (descriptors + serial + capacity). A mismatch halts without writes. `pm.s3_require_rtc` stops unattended S3s during bring-up. |
| Hang in S3 or during resume (no serial) | RTC-only wake. Power button is the manual wake. A 4 s hold forces power off. The one-shot Limine entry boots the good kernel next time. Breadcrumbs (beep / LED / stick klog). |
| `_PTS` left the EC in "sleeping" (fans off, keyboard dead) after an aborted suspend | `_WAK(3)` always runs once `_PTS` has run (error path). Never leave the laptop unattended in H2-H4. Last resort is the HP EC reset (AC off, hold power 15 s). |
| Firmware scribbles on low RAM during resume | Wake code lives in the page SMP already proves free. Buddy is checked to hand out no other <1 MiB page. Canaries in low memory. |
| Waking vector left set | Cleared in R5 and on every error path. It only matters to the firmware on an S3 resume. |
| Overheating in a bag (suspend failed silently with the lid closed) | Lid policy defaults to `ignore`. `pm.s3_require_rtc` is on until H6 passes. |
| aarch64 or other x86 boxes affected | `cfg(x86_64)`, `pm.s3=1` required, and both-arch regress per merge batch. |

---------------------------------------------------------------------------------------------------
## 8. Lane split (each ≤ 60 tool calls; aarch64-only testing per lane, both arches per merge batch)

| # | Lane | Scope | Depends on |
|---|---|---|---|
| 1 | **s3aml** | `acpi_power::{sleep_prepare, sleep_wake, tts, sx_typ, pause_poll}`; gaps 1-6 of §3.1 (dropped-space counters, CMOS read, Sleep honour, global lock, amltrace); offline harness over the laptop DSDT | lapdsdt (EC), lapi2c (`with_ns`) merged |
| 2 | **s3sysfs** | `/sys/power/{state,mem_sleep,pm_test,wakeup_count}`, `/sys/class/rtc/rtc0/{wakealarm,since_epoch}`, CMOS alarm program/clear (§1.3), knobs parsing, `pm_test` freezer/devices/platform plumbing, suspendtest `--level` | none (parallel with 1) |
| 3 | **s3core** | FACS map + vector, `acpi_enter_s3` (§1.1), wake stub @0x7400 + temp root < 4 GiB, `pm_s3_lowlevel`/`pm_s3_resume64`, CPU state, PIC/IOAPIC/LAPIC, TSC rebase of all raw-TSC state, realtime/boottime, verify CRC, beep/LED, i8042 MEM resume, `pm_test=core`; **refuses unless 1 CPU online** (adds `nosmp` knob) | 1, 2 |
| 4 | **s3cpus** | freezer; AP park in idle (sched/src/lib.rs:7596), INIT, resume SIPI → `sched_ap_resume_entry` + longjmp; TSC_ADJUST sync; lifts the 1-CPU limit; `pm_test=processors` | 3 |
| 5 | **s3pci** | generic PCI save/restore incl. MSI-X tables, PCIe/LTR/L1SS caps; virtio re-init for QEMU (blk, net; gpu stays dark) → first **real S3 round trip in QEMU** | 3 |
| 6 | **s3xhci** | xHCI CSS/CRS save/restore + reinit + USB persist reattach for usb_storage/usb_hid, UNIT ATTENTION handling; QEMU `--usb-stick` S3 ×20 | 5 |
| 7 | **s3nvme** | NVMe bring_up refactor + resume (HMB/APST); e1000e ULP-disable + re-init | 5 (parallel with 6) |
| 8 | **s3i915** | display + GT resume (wa, GGTT, engines, RC6/RPS, DMC, modeset/eDP PPS/backlight); `pm.s3_i915=skip` knob from day 1 | 5 (parallel) |
| 9 | **s3iwlhda** | iwlwifi fw reload + carrier events; HDA CRST/CORB/RIRB + codec verb replay | 5, 8 (HDMI codec after i915) |
| 10 | **s3policy** | login1 CanSuspend/Suspend/PrepareForSleep + delay inhibitors, init `suspend`, lid evdev + logind.conf (default ignore), post-resume lid debounce; phase-2 wake sources (`_PRW`/`_DSW`, PWRBTN/LID GPE) behind a knob | 2, 4 |

**Order:** 1 ∥ 2 → 3 → 4 ∥ 5 → 6 ∥ 7 ∥ 8 → 9 → 10.

Hardware gates:
- H0-H2 after lane 1.
- H3 after lane 4.
- **H4 (first real S3 on the laptop) needs lanes 1-6.** Wi-Fi and the display are not required, because
  evidence goes to the stick klog.
- H5 after lane 7 or 9.
- H6 after lanes 8-10.

Every lane runs the regress and Q0 (both arches inert) before it hands back.
