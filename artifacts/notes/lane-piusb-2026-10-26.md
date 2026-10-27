# lane/piusb — Pi 5 USB via RP1's two DWC3 controllers (2026-10-26)

Status: PARTIAL (implemented, host-tested, QEMU virt green incl. forced non-coherent mode; NOT hardware-verified).

## What
- `drivers/src/dwc3_hw.rs` — DWC3 globals math (Linux rpi-6.12.y core.{c,h}): GSNPSID check, GCTL host
  (PRTCAPDIR=1), core+PHY soft reset, `snps,axi-pipe-limit=8` (GSBUSCFG1.PIPETRANSLIMIT=7),
  `tx-thr-num-pkt=2`/`tx-max-burst=8` (GTXTHRCFG PKTCNTSEL|2<<24|8<<16), GUCTL1 NAK_PER_ENH_FS(19) +
  PARKMODE_DISABLE_SS(17)/FSLS(15), GUSB3PIPECTL DISRXDETINP3(28). Host tests `drivers/dwc3-tests` 6/6.
  No GFLADJ change (DTB has no ref clock / frame-length quirk); value is logged.
- `drivers/src/rp1_usb.rs` — `attach(bar1)` from `rp1::probe` (the one line in rp1.rs): VBUS on (RP1 GPIO 42
  USB_VBUS_EN as SYS_RIO output high), glue usb0 (BAR1+0x200000, RP1 irq 31) and usb1 (BAR1+0x300000, irq 36),
  queue the ones in host mode. `probe_pending()` from `usb_hcd::init`: DMA offset = `rp1::dma_addr(0)`
  (0x10_0000_0000), `usb::probe_xhci(base, 0xc100)`, register in usb_hcd's controller list so
  `quiesce_all` halts+resets them before kexec (and turns RP1 01:00.0 bus mastering off).
- `usb::platform::set_dma_offset` (default 0): `DmaBuf::phys()` = CPU phys + offset = device address.
  Every TRB/context/DCBAA pointer goes through it; no other device-address source exists in the stack.
- #103 cache maintenance: `DmaBuf::new` on a non-coherent platform cleans+invalidates the linear alias and
  maps the buffer Normal Non-cacheable (Linux dma_alloc_coherent model): rings, ERST, DCBAA, scratchpad,
  contexts, control/HID buffers need no per-access maintenance; their sync_* become barriers. Bulk bounce
  buffers use `DmaBuf::new_streaming` (cacheable) with the existing clean-before / invalidate-after syncs.
  Coherent platforms (x86_64, QEMU virt) take the old path unchanged.
- IRQ: stack stays polled. With `rp1.irq=1`, interrupter 0 + USBCMD.INTE are enabled and the RP1 source is
  wired over MSI-X (edge); the ISR only acks (USBSTS.EINT, IMAN.IP) and counts, logging the first one.

## Expected Pi lines (cmdline `pcie.rpi5=1`, after the existing [pcie]/[rp1]/[macb] lines)
```
[rp1usb] vbus on (gpio42)
[rp1usb] usb0 snpsid=0x5533....  gctl=0x...1... gsts=0x...1 gfladj=0x... host
[rp1usb] usb1 snpsid=0x5533....  ... host
[XHCI] bus 1: xHCI 1.10, N ports, ... 64-bit DMA ...
[rp1usb] usb0 xhci up, devices=0x.. irq none (polled)
[USB] Bus 001 Device 001: ID ....   (one per plugged device)
```
(with `rp1.irq=1`: `irq msix`, and `[rp1usb] usb0 first irq` on the first event after boot.)
Failure forms: `[rp1usb] FAIL bar1 too small`, `usbN snpsid=.. FAIL not dwc3`, `.. FAIL not in host mode`,
`[rp1usb] usbN FAIL xhci: <reason>`, or `[XHCI] ... timeout` lines from the stack.

## Evidence
- dwc3-tests 6/6; check-rpi5.sh rpi5 + raspi4b clean (no warnings in touched files).
- aarch64 virt `usbunplugtest all` PASS; again with `dma.coherent=0` (boot: `[early] dma: NON-COHERENT`) → all
  phases PASS (unplug/notready/fastreplug/hub/hid/emptyhot/readers): the NC DmaBuf path, its iounmap on drop and
  the streaming bounce path work on a running xHCI.
- Box full aarch64 regress: see merge commit.

## Open / unverified
- RP1 clocks for the USB blocks are left at RP1 reset defaults (lane pirp1 owns clocks). If `snpsid` reads 0
  or all-ones, that is the first suspect.
- VBUS via plain GPIO (not the `vbus1` alt function); USB_OC_N (gpio43) not monitored.
- GUSB2PHYCFG left at reset (no SUSPHY changes); no GFLADJ/REFCLK programming.
- Interrupt mode is diagnostic only (the stack has no IRQ-driven completion path on any platform).
