# lane/lapsusp 2026-10-27: suspend to RAM, step 1 (PARTIAL)

## Done (step 1): freeze-only device cycle (Linux pm_test=devices)
- kernel/src/pm.rs: hook table in suspend order i8042 -> i915 -> e1000e -> block -> xhci; resume reversed;
  a failing suspend resumes what already ran (dpm_suspend error path). Trigger: reboot(2) private cmd
  0x504D5453 ("PMTS"), arg = {flags, hold_ms, buf, buf_len}; root only. Inert unless called.
- Hooks: i8042 (0xAD/0xA7 + CTR save, 0xAE/0xA8 restore), i915 plane 1A off/on (waits SURFLIVE),
  e1000e (RUNNING off, RINGS lock, IMC, RCTL/TCTL EN off; restore), blkdev gate (new requests wait,
  bounded 10 s; drain in-flight; flush all write caches), xHCI (default: USBCMD.INTE off; flag 1: R/S halt).
- boot/src/acpi.rs logs `[ACPI] sleep: LOW_POWER_S0= \_S3_= SLP_TYP= FACS= _PTS= _WAK=`.
- userland/suspendtest (C, zig): pm_cycle, io_during (child write+fsync x200 through the cycle),
  disk_after (1 MiB write/fsync/readback), net_after (e1000e link up; --net IP:PORT connects), display_after.
- QEMU: x86_64 (TCG) 2 runs incl. --xhci-halt --hold 1000: all PASS, quiesce 3.5 ms, writer fsync held
  up to 1008 ms then completed, bad=0. aarch64: suspendtest --xhci-halt + vfstest PASS.
  QEMU had no e1000e/i915 and 0 registered xHCI, so those hooks are only "not present"-tested.
  QEMU x86 FADT: LOW_POWER_S0=0, \_S3_ typ 1/1.

## NOT done
- Real laptop: never ran. 172.16.149.179 was unreachable (ssh "Host is down") for the whole window;
  nothing was deployed (upload failed before arming). Laptop state unknown (not caused by this lane).
- Step 2 (real S3) not started.

## Design for S3 (next lanes)
1. Laptop facts first: boot this kernel, grep `[ACPI] sleep`. EliteBook 840 G5 is expected to expose
   \_S3_ with LOW_POWER_S0=0 (S3 preferred). If LOW_POWER_S0=1 go s2idle (freeze + MWAIT deepest C-state,
   wake on SCI/GPE) instead.
2. AML gap: we have no interpreter; _PTS(3)/_WAK(3) (HP EC, \_SB.PCI0 power resources, GPE wake arming)
   cannot run. Options: (a) port an AML interpreter (acpica-style, or the `acpi`/`aml` Rust crates) — needed
   anyway by lappower for _BST/_BIX; (b) try S3 without _PTS (many machines resume; HP EC may not).
   Choose (a), shared with lappower.
3. Entry: freeze tasks (new: park user threads at syscall return), device hooks with MEM semantics
   (power loss), offline APs (INIT IPI), save BSP state (CRs, GDT/IDT/TR, MSRs EFER/STAR/LSTAR/GS/FS/PAT,
   x2APIC/LAPIC/IOAPIC, HPET/TSC deadline), write FACS.firmware_waking_vector = real-mode trampoline
   (<1 MiB, reuse the AP/kexec trampoline: 16 -> 32 -> 64 bit, load saved CR3), wbinvd, write
   PM1a_CNT SLP_TYP(\_S3_)|SLP_EN. Resume: trampoline -> restore state -> re-init LAPIC/IOAPIC/timers ->
   SIPI APs -> device resume -> thaw.
4. MEM-semantics device resume (state lost): NVMe full re-init (CC.EN, admin+IO queues; keep namespaces),
   xHCI save/restore (USBCMD CSS/CRS, xHCI 4.23.2) else HC reset + re-enumerate (root on USB stick:
   must use CSS/CRS!), i915 re-init display (modeset restore from atomic state) + GT (execlists, GGTT
   rewrite), HDA CRST + codec re-config (pipewire_server), e1000e PHY/MAC re-init (reuse init path),
   iwlwifi fw reload + reassociate (NetworkManager can drive it), i8042 reset + touchpad re-init.
5. Userspace path: systemd-logind/systemctl suspend writes "mem" to /sys/power/state (systemd-sleep);
   add /sys/power/{state,mem_sleep,pm_test,wakeup_count} in servers/vfs mapped to pm::freeze_cycle /
   pm::suspend_mem. COSMIC calls logind Suspend over D-Bus (our logind shim must forward).
   Lid-close policy: logind HandleLidSwitch=suspend once lappower delivers the lid switch.
6. Safety on hardware: only RTC-alarm wake (CMOS RTC alarm + RTC_EN in PM1_EN, 20 s), and only after
   suspendtest passes on the laptop.

## Commits
lane/lapsusp: pm framework + hooks + suspendtest; merge of main e5445518.
