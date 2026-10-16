# Lane rebootq — 2026-10-16

Branch `lane/rebootq` off local main `a34df0a3`. Not pushed. Real-hardware
target: HP EliteBook 840 G5 (i5-8350U, Sunrise Point-LP PCH, Conexant CX8200
codec 14f1:2008, xHCI 8086:9d2f + Alpine Ridge 8086:15db, i915, e1000e,
iwlwifi 8265, USB-stick root).

## What the user's facts changed (mid-lane)

1. **The "warm reboot falls through to Arch" is HP Fast Boot**, not our reset.
   With Fast Boot ON the firmware skips USB enumeration and boots NVMe unless
   F9 is used; the F9 menu pre-selects the stick. So reset-method work is the
   lowest priority (kept, defaults changed carefully, all knobs).
2. **With Fast Boot OFF, LeandrOS (factory and trial) shows the Limine menu,
   then the screen goes black immediately and no text ever appears.** This is
   now the top problem. Nothing was known about how far the kernel gets.
3. **The HDA beep after a LeandrOS warm reboot is real** and Linux's
   shutdown silenced it. Fixed here (codec quiesce).

## A. What LeandrOS did on reboot/poweroff (before this lane)

`reboot(8)` → init → `reboot(2)` → `kernel/src/power.rs::kernel_restart`:
fs sync + f2fs clean-unmount checkpoint, NVMe CC.SHN, console drain, klog
final flush, `cli`, then `arch_x86_64::power::restart`: FADT RESET_REG
(I/O 0xCF9 <- 0x06 on the HP: hard reset, no power cycle), 8042 0xFE x10,
0xCF9 0x02/0x06, triple fault. **No device shutdown at all**: the HDA stream
kept DMAing (it runs continuously, silence included) with the codec in D0,
EAPD on for every EAPD pin (0x16, 0x17, 0x1d, 0x22 — including two unused
pins) and the speaker pin output-enabled; xHCI left running (R/S=1); the USB
stick never got a SYNCHRONIZE CACHE after the klog's final write; every bus
master left on. The other CPUs keep running (no smp_send_stop) — unchanged.

Linux on this machine (no DMI quirk for the 840 G5; `reboot_type =
BOOT_ACPI`; `efi_reboot_required()` false — not HW-reduced): the same FADT
write (0xCF9 <- 0x06), but after `device_shutdown`: `cx_auto_shutdown`
(Conexant `reboot_notify`: EAPD off, `snd_hda_shutup_pins`, all widgets and
the AFG to D3 — the comment says "to avoid spurious noises from the internal
speaker during (and after) reboot"), `azx_shutdown` (stop chip, link reset
deliberately skipped: `__azx_shutdown_chip(chip, true)`), `xhci_shutdown`
(halt; HCRST only with XHCI_SPURIOUS_WAKEUP/RESET_TO_DEFAULT, not this PCH)
+ `pci_disable_device`, `sd_shutdown` (SYNCHRONIZE CACHE), e1000e
`__e1000_shutdown` (release DRV_LOAD, `pci_clear_master`), nvme shutdown.

Our HDA driver never sends SET_BEEP_CONTROL (0x70A) and never touches the
beep widget (nid 0x12) — the beep generator is refuted as the source.

## B. Implemented

Kernel reboot/poweroff now runs (kernel/src/power.rs):
1. `shutdown_prepare` (unchanged: fs, f2fs, NVMe).
2. **Phase 1** (IRQs on, logged to klog so it reaches the stick):
   `[REBOOT] restart: quiesce=1 type=cold method=auto ...` plan line;
   audio (`pipewire_server::shutdown` → `Hda::shutdown`: all stream
   descriptors stopped, EAPD off on every EAPD pin, every pin's out-amp
   muted and PIN_WIDGET_CONTROL 0, beep generator off + muted, converters
   unbound, every power-manageable widget then the AFG to D3 with PS-Act
   read back, INTCTL/CORB/RIRB/DPL off; 300 ms budget); e1000e (IMC all, RX/TX
   off, DRV_LOAD released); collect network/display functions for BME off.
3. klog final flush, then **SYNCHRONIZE CACHE on every writable whole disk**
   (`blkdev::flush_all_disks`), then `cli`.
4. **Phase 2** (serial only): every xHCI halted (32 ms bound), HCRST (Intel
   1 ms post-write delay, 250 ms bound), optional root-port power off, bus
   master off; then bus master off on audio/network/display functions.
5. Reset (x86_64), see knobs. aarch64 keeps PSCI, same quiesce (no-ops in
   QEMU apart from BME on virtio functions).
6. `halt` silences the audio only.

Fast-Boot-off black screen work (kernel/src/main.rs, arch/x86_64/src/lib.rs,
boot/src/earlyfb.rs, drivers/src/usb_hcd.rs, arch/x86_64/src/keyboard.rs):
- **earlyfb stage squares** (x86_64, default on, `earlyfb=0` off): 20x20
  squares at the top-left of Limine's framebuffer, one per stage, colours
  cycling white, red, green, blue, yellow, cyan, magenta, orange:
  1 kernel entered · 2 mm up · 3 GDT/IDT/SSE · 4 fb mapped + CPUs enumerated
  · 5 LAPIC+IOAPIC · 6 timer calibrated · 7 syscall/kernel root · 8 APs
  started · 9 FADT read · 10 early USB handoff · 11 i8042 probed · 12 ACPI
  touchpad scan · 13 RTC/kfacil · 14 PCI ready · 15 about to start the
  console. The console clears them on a normal boot.
- `[BOOT] limine framebuffer: N reported, using #0 WxH pitch P bpp B model M
  masks ...` once the console is up — in the RAM ring, so it reaches the
  stick's persistent log whenever the boot gets as far as USB storage
  (compare Fast Boot on vs off).
- **Early USB BIOS handoff** before the i8042 probe (Linux
  `quirk_usb_early_handoff` order): every xHCI's USBLEGSUP (OS-owned, wait
  1 s, SMIs off) and every EHCI's; logged `[XHCI] 00:14.0: early BIOS
  handoff: USBLEGSUP ... (BIOS released it|not BIOS-owned|...)`.
  `usb.early_handoff=0` restores the old order.
- `i8042.off=1`: no i8042 probe at boot (the post-handoff `recheck` still
  probes).
- `fbcon=0`: the framebuffer console is not started (serial + klog only).
- SMP guard: if 0x7000-0x7FFF is not usable RAM in the memory map, boot on
  the BSP only and log `[SMP] trampoline page 0x7000 is not usable RAM ...`
  (writing it through the HHDM would page-fault before any console exists).

Knobs (kernel command line):

| knob | default | meaning |
|---|---|---|
| `reboot.quiesce=0` | 1 | skip phases 1/2 (old behaviour) |
| `reboot.type=warm` | cold | cold: 0xCF9 0x0E full reset first (power cycle; EDK2's EfiResetCold on Intel PCH), then ACPI, 8042, triple. warm: Linux chain ACPI, 8042, ACPI, 8042, 0xCF9 0x06, triple |
| `reboot.method=acpi\|cf9\|kbd\|triple` | auto | try this first, then the rest of the chain |
| `reboot.hda_crst=1` | 0 | also hold the HDA link in reset (Linux does not) |
| `reboot.xhci_hcrst=0` | 1 | halt only |
| `reboot.usb_vbus=off` | on | root-port power off (only if HCCPARAMS1.PPC=1; logged) |
| `reboot.busmaster=0` | 1 | keep bus mastering |
| `earlyfb=0` | on | no stage squares |
| `usb.early_handoff=0` | on | handoff at xHCI probe again |
| `i8042.off=1` | off | no boot-time i8042 probe |
| `fbcon=0` | on | no framebuffer console |
| `i915.off=1` / `i915.kms=0` (existing) | | i915 untouched / no KMS |

Why cold by default: the HP's FADT reset register *is* 0xCF9 with 0x06 (hard
reset without power cycle), so Linux's "ACPI first" and ours differ only by
FULL_RST. A full reset power-cycles the codec and (on most boards) USB VBUS,
which is the only way to clear analog codec state that survives PLTRST# and
Linux's own probe (the beep did). Reboot takes ~3-5 s longer.

EFI ResetSystem / BootNext (`reboot.bootnext`): **not implemented**. This
kernel has no EFI runtime services: Limine does not call
SetVirtualAddressMap, there is no system-table request, and nothing maps the
runtime code/data/MMIO regions. Doing it means an identity map of those
regions plus a firmware call with interrupts off — a lane of its own. Check
first whether HP Fast Boot even honours BootNext (matrix step F below).

## C. QEMU

See the commit message / handback for the run results.
`.claude/skills/run-leandros/reboottest.py [arch]`: boot, `reboot`, assert the
`[REBOOT]` lines and a second boot in the same QEMU, `poweroff`, assert QEMU
exits.

## D. Laptop test matrix (with the user; cold boot after each failure)

Fast Boot OFF first (the black screen), then the beep.

1. **Fast Boot OFF, trial = this kernel, no extra knobs.** Pick the trial
   in Limine. Photograph the top-left corner as soon as the screen changes.
   - squares appear and stop at N → the stage after N hangs (table above);
   - 15 squares then text → the console works; if text then stops, it is
     later (USB, i915 ...): wait 2 min, probe SSH;
   - no squares at all → stage 1 never ran (Limine handoff / the kernel died
     at entry) or the framebuffer Limine reports is not the one on the panel
     (GOP mode/output mismatch).
2. If it hangs at 10 or 11: `usb.early_handoff=0` and separately
   `i8042.off=1` (and both).
3. If the console text appears then the screen goes black: `i915.kms=0`,
   then `i915.off=1`.
4. If no squares: `earlyfb=0 fbcon=0` (serial/klog only) and wait for SSH —
   tells "alive but invisible" from "dead".
5. After the next good boot (Fast Boot ON + F9, or a working knob set):
   `scripts/remote.py logs` (or on the laptop `espctl klog --all`; or on the
   Mac with the stick: `sudo ./scripts/read-usb-log.sh /dev/rdiskN --all`).
   A Fast-Boot-off slot exists only if the boot reached USB storage; then
   compare its `[BOOT] limine framebuffer`, `[MEMMAP]`, `[XHCI] ... early
   BIOS handoff` and `[SMP]` lines with a Fast-Boot-on slot.
6. **Beep**: Fast Boot ON, boot LeandrOS via F9, play something (or not),
   `reboot`. Listen through the reset. Then `remote.py logs` on the next
   boot and check the previous slot's tail:
   `[REBOOT] restart: quiesce=1 type=cold ...`,
   `[REBOOT] audio: HD Audio: streams stopped (1 were running); codec
   14f1:2008: EAPD off on 4 pin(s), ... AFG 0x01 D3 sent (PS-Act D3) ...`,
   `[REBOOT] phase 1 done ...`, `reboot: Restarting system`.
   Expect: no beep; machine power-cycles (screen off ~3-5 s).
7. Beep A/B (only if 6 still beeps): `reboot.type=warm` (codec quiesce but
   no power cycle), then `reboot.quiesce=0` (old behaviour) to confirm the
   repro, then `reboot.hda_crst=1`.
8. Fast Boot policy: from Arch, `efibootmgr --bootnext 0003 && systemctl
   reboot` with Fast Boot ON — if that boots the stick, implementing BootNext
   in LeandrOS is worth a lane; if not, BootNext is useless with Fast Boot.

## Hypotheses, ranked

Fast-Boot-off black screen (Limine menu shows, then black at once):
1. **Kernel dies before the console** on something the full firmware init
   changes: (a) memory map — the fixed SMP trampoline at 0x7000 written
   through the HHDM faults if that page is not usable RAM (guarded now), or
   another HHDM access to a region Limine does not map; (b) SMM USB legacy
   emulation active during the i8042 probe (handoff now first). The squares
   localize it.
2. **GOP / framebuffer mismatch**: firmware sets a different mode or output;
   Limine's framebuffer is valid but our console draws something invisible,
   or the panel is driven by a pipe the GOP buffer is not on. Squares absent
   but SSH alive → this. `[BOOT] limine framebuffer` line compares modes.
3. **i915 KMS** taking over the pipe — only if text appears first (i915
   starts long after the console).

HDA beep:
1. Codec left in D0 with EAPD on (all four EAPD pins) and the DAC streaming
   when PLTRST# hit; the Conexant analog path (integrated class-D amplifier)
   latched a tone that link reset and Linux's probe do not clear but AFG D3
   does — exactly what `cx_auto_shutdown` exists for. Fixed by the codec
   quiesce; the cold reset additionally power-cycles the codec.
2. PCBEEP passthrough of a driven beep line — unlikely (port 0x61 gate
   clear).
3. Beep generator — refuted (never programmed; link reset clears it).

Reboot fallthrough to Arch: HP Fast Boot (user-confirmed). Quiesce + cold
reset are hygiene, not the fix.
