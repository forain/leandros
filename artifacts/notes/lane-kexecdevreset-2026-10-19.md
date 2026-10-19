# lane/kexecdevreset (2026-10-19)

Before this lane, `reboot(KEXEC)` reset only virtio devices. Every other PCI
function only lost bus mastering and MSI/MSI-X. A real NIC, GPU or xHCI
could keep state, or keep running, behind the next kernel's back. This lane
adds a real device reset for kexec. Restart and power-off behave as before.

## Where drivers live
All hardware drivers are in-kernel (`drivers/src`: nvme, usb_hcd/xHCI, hda,
e1000e, r8169, iwlwifi, i915, nvidia_probe, virtio_*). The servers
(f2fs, pipewire, net) are in-kernel crates too. The shutdown hooks that
already existed (power.rs): f2fs clean unmount, NVMe CC.SHN, pipewire HDA
shutdown (codec D3, optional CRST), e1000e RX/TX off, and xHCI halt + HCRST.
This kernel has no IOMMU code (no DMAR/IORT, VT-d and SMMU are never
enabled), so there is nothing to turn off. Translation stays as the firmware
left it.

## Sequence (power.rs `kernel_kexec`)
1. `shutdown_prepare`: sync, f2fs clean unmount, NVMe CC.SHN.
2. `quiesce_phase1`: audio, now with the HDA link **held in reset (CRST)
   by default** for kexec (`reboot.hda_crst=0` turns that off), and e1000e.
3. `kexec::devreset::plan` (task context): pick a reset method per function
   and store it in a static table. It also decides which function keeps
   bus mastering (see below).
4. `machine_prepare` (klog final flush, disk flush, interrupts off), then
   `quiesce_phase2` (xHCI halt + HCRST, bus master off).
5. `kexec_hooks` (new, kexec only): `gpu_kms::console_restore` (scanout back
   on the GOP surface), NVMe **CC.EN=0** and wait for RDY=0, **i915** rings
   stopped (MI_MODE.STOP_RING) + GT reset (GRDOM_FULL; display untouched),
   **r8169** IRQs masked, RX/TX off, ChipCmd.RST, **iwlwifi** IRQs masked,
   STOP_MASTER, SW_RESET (no retake, busy-waits only).
6. `quiesce_all_pci`: virtio status=0, MSI/MSI-X off, bus master off.
7. The arch code stops the other CPUs and masks the IOAPIC/GIC. Then, on
   the last CPU, `devreset::execute` runs with no locks and no allocation.
   It uses raw CF8/ECAM config access and records every function's header.
   It sets INTx disable, starts every reset at once, waits once (10 ms
   D3hot/SBR dwell, then 100 ms settle), and polls each function back
   (CRS / all-ones, up to 1 s). It then restores BARs, ROM, cache line, the
   INTx line and PCIe DevCtl/LnkCtl/DevCtl2. Command = IO|MEM|PERR|SERR, so
   bus mastering is off and INTx is enabled again. One log line per function.

## Reset method per function (Linux `pci_reset_function` order)
- FLR (PCIe DevCap.FLR; waits for TransactionsPending first).
- AF FLR (Advanced Features cap with TP+FLR: Intel integrated HDA, xHCI,
  IGD, I219 have no PCIe cap).
- D3hot→D0 if PMCSR.No_Soft_Reset is clear and the function is in D0.
- Secondary bus reset of the parent bridge. This is used only when every
  function on that bus is itself being reset, so none is virtio, firmware
  owned, a bridge or the framebuffer. `kexec.sbr=0` disables it.
- Otherwise the function is not reset and relies on its driver hook.

Never reset: bridges (class 06 or header type 1: host bridge, root ports,
LPC), functions outside the reset classes (01 storage, 02 network,
03 display, 04 multimedia, 0C03 USB, 0805 SD, 0D wireless), virtio, and
the **framebuffer owner**. Firmware-owned functions (MEI, SMBus, ...) keep
INTx disabled. Reset-class and virtio functions get INTx back, because the
aarch64 drivers use INTx and never clear the bit. Linux `quirk_no_flr`
IDs skip FLR.

## Framebuffer
x86_64 passes MB2 tag 8. aarch64 passes no framebuffer (FDT has no
simple-framebuffer), so nothing is spared there. `devreset::handover_fb`
now decides for both info.rs and the plan. The framebuffer is passed when
it lies in a class-03 function's memory BAR or above all RAM. The BAR test
works without sizing: it takes the largest BAR base ≤ fb, and fb must lie
within that base's natural alignment.
That function is not reset. It keeps bus mastering only if it is an Intel
IGD: its scanout reads stolen memory, and its GT was reset by the hook.
`kexec.fbbm=0` clears it anyway. Any other owner loses bus mastering
(NVIDIA: GSP message queues live in system memory). `kexec.fbreset=1`
resets the owner too and hands over no framebuffer.

**Behaviour change**: the old rule (fb above all RAM) refused the laptop's
GMADR framebuffer (it sits below 4 GiB with 16 GiB RAM) and QEMU q35's
virtio-vga at 0x80000000. Both are now handed over, and gen 1 and gen 2
print on the boot framebuffer (screenshot checked, x86_64).

Knobs: `kexec.devreset=0`, `kexec.sbr=0`, `kexec.fbreset=1`, `kexec.fbbm=0`,
`reboot.hda_crst=0`, and `reboot.quiesce=0` (skips the hooks; the
generic resets still run).

## Tests (QEMU TCG, 4 CPUs, this lane's kernels in a cloned ESP)
Extra devices: `nvme` (raw disk holding kernel-direct), `e1000e`,
`qemu-xhci` + usb-kbd, `ich9-intel-hda` + `hda-duplex`; on x86_64 a second
`qemu-xhci` behind a `pcie-root-port`. Chain: Limine → `kexec -l` of the
kernel read from the NVMe disk → gen 1 → the same again → gen 2.
Scripts: scratchpad `chain.sh`, `mkimg.sh`, `kbuild.sh`.
- aarch64 uefi-tcg: NVMe **FLR** (back after 110 ms), e1000e **D3hot→D0**.
  xHCI and HDA have no FLR/PM in QEMU and get their hooks only (HCRST, CRST).
  Gen 1 and gen 2: `kexec.gen=1 kexec.gen=2`, nproc 4, login. The kernel read
  back from nvme0n1 matches md5. e1000e link up with packets. HDA codec probed,
  xHCI keyboard enumerated. The gen1→gen2 jump was called from cpu1.
- x86_64 (TCG): same results, plus the root-port xHCI 01:00.0 had a
  **secondary bus reset** through bridge 00:0b.0 and was re-probed as
  bus 2 in each generation. virtio-vga was kept as the framebuffer owner.
  Reset total 116 ms.

## Open
- Real hardware is untested. On the HP, i915 is the framebuffer owner (not
  reset; rings stopped + GT reset), and the integrated HDA/xHCI/I219 should
  take AF FLR. Check the per-function `[KEXEC]` lines on the first laptop
  kexec.
- Intel IGD scanout with BME cleared is unverified, which is why it keeps
  bus mastering by default. Test `kexec.fbbm=0` on the laptop.
- NVIDIA/GSP as framebuffer owner on the PC: no driver hook. GSP keeps
  running with bus mastering off, and the next kernel's nvidia probe would
  meet a booted GSP. Use `kexec.fbreset=1` (SBR on the root port, no early
  framebuffer) until a GSP unload hook exists.
- Extended config beyond the PCIe control registers (LTR, L1SS, AER masks)
  is not restored after a reset. Devices work without it but lose ASPM L1
  substates until their driver programs them.
- Other CPUs keep running during the hooks (pre-existing kexec design). A
  compositor flip after `console_restore` is undone by a second
  `console_restore` on the last CPU.
- The aarch64 PCI scan covers bus 0 only, so SBR never applies there.
