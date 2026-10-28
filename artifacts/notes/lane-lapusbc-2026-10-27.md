# lane lapusbc - USB-C ports via the Alpine Ridge xHCI (2026-10-27)

Status: PARTIAL (code merged, QEMU green on both arches; laptop trial NOT run - laptop 172.16.149.179 was down/unreachable).

Implemented per design-lapusbc-2026-10-27.md:
- D1: `[XHCI] bus N root hub: USB3 ports a-b, USB2 ports c-d; PORTSC p1 0x.. ...` (drivers/usb/src/xhci/mod.rs `log_root_hub`).
- D2: `controller_gone` (USBSTS == all-ones) guard in `hotplug_scan`; logs `[USB] bus N controller gone ...` once, detaches root-port devices, pauses scan; `controller back` log only. Knob `usb.tbxhci=0` (kernel/src/main.rs -> `usb_hcd::disable_controller_gone_guard`).
- D3: lsusb lists one synthetic root hub per controller (1d6b:0003/0002, ROLE_ROOT_HUB 0x40); userland/lsusb prints "Linux Foundation 3.0/2.0 root hub". Root hub shows as Device 001 even if a real device also has address 1 (accepted in design).

Evidence (QEMU x86_64 with qemu-xhci via LEANDROS_USB_INPUT=kbd,mouse):
`[XHCI] bus 1 root hub: USB3 ports 1-4, USB2 ports 5-8; PORTSC p1 0x202a0 ...` and
`Bus 001 Device 001: ID 1d6b:0003 super-speed - Linux Foundation 3.0 root hub`. aarch64 boots, /bin/lsusb RC=0 (no xHCI in default aarch64 QEMU).

Laptop steps still to run (lock.sh laptop, INTEL=1 trial, boot good back, no promote):
1. `dmesg | grep "root hub"` -> expect bus 2 line (3a:00.0). 2. `lsusb` -> Bus 002 root hub. 3. 10 min idle: no `bus 2 controller gone`.
4. Needs user: plug a USB3 stick in the USB-C port -> expect it on bus 2.

Incident: disk filled (other lanes) during aarch64 test and zeroed worktree aarch64_vars.fd (pflash "requires 67108864 bytes ... provides 0"); deleting the file fixes it.
