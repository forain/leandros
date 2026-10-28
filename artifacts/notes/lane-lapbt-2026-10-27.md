# lane lapbt — Intel 8265 Bluetooth over USB (2026-10-27)

Status: see "Laptop evidence" below.

## What landed
- `drivers/usb/src/xhci/btusb.rs`: E0/01/01 HCI interface scan; enumeration sets the
  configuration and hands the function over (same path as audio/UVC); `BtUsb` opens
  interrupt IN + bulk IN/OUT, keeps 16 one-packet TDs queued per IN endpoint, non-blocking
  `poll`, commands on EP0 (class request 0x20/0), ACL + bootloader secure-send on bulk OUT.
  Endpoint errors recover with `recover_ep_inner` (never kills the controller: root is on USB).
- `drivers/src/bt/usb_reasm.rs`: H4 reassembly from USB chunks (event/ACL length fields).
- `drivers/src/bt/btintel.rs`: legacy secure bootloader flow (Read Version 0xfc05, Boot
  Params 0xfc0d, RSA .sfi secure send 0xfc09 header 128/256/256 + command stream in
  4-byte-aligned fragments, wait ff/06, Intel Reset 0xfc01 with the 0xfc0e boot address,
  wait ff/02, Read Version = operational, DDC 0xfc8b, event mask 0xfc52). Already-operational
  controllers (warm reboot/kexec) skip the download.
- `drivers/src/bt/usb.rs`: kthread `btusb` (only spawned when a BT function was enumerated),
  set-up, then registers `/dev/hci0` via the shared `hcidev.rs` (no longer rpi5-only) and pumps.
  Knobs: `bt=0`, `btusb.fw=0`.
- Firmware: `ports/intel-bt/fetch.sh` -> `~/.cache/leandros/intel-bt` (ibt-12-16.sfi/.ddc,
  linux-firmware 20240909, sha256-pinned) -> `/lib/firmware/intel/` on the x86_64 root image.
- Host tests (drivers/bt-tests, `usb_tests.rs`, 10 tests): reassembly, .sfi walk (synthetic +
  the real ibt-12-16.sfi when cached: 2356 payload fragments, boot 0x40800), full set-up
  against a scripted controller (download, reject, missing fw, already operational).

## Laptop evidence (trial 3f262152)
```
[btusb] controller 8087:0a2b on slot 1
[USB-BT] slot 1: HCI iface 0 evt 81 (mps 64, ival 1) acl in 82 (mps 64) out 02 (mps 64)
[btusb] version: platform 0x37 variant 0x0c rev 0x12 fw bootloader ...
[btusb] boot params: dev_revid 16 secure_boot 1 otp bdaddr B4:69:21:8C:5A:7C
[btusb] ibt-12-16.sfi: 584092 bytes in 2361 fragments, 1331 ms, boot address 0x00040800
[USB-BT] slot 1 dci 5: transfer error cc 4, resetting endpoint   (bulk IN during Intel Reset; recovered)
[btusb] booted: ... fw operational 0.19.44.2021
[btusb] ibt-12-16.ddc: 3 records applied
[btusb] /dev/hci0 ready
```
`/dev/hci0` = crw------- 10,137.

## Found
- An exited kthread breaks `ps` (pstest "ps -ax", "ps x", "ps -e lists pid 1" FAIL); repro:
  a kthread whose closure returns. Worked around here (thread parks). Kernel bug open.
- The laptop's root image has no /bin/btd, btctl, bthid (old image); pushed to /root/bt for the test.

## Open (PARTIAL)
- `btctl show` / `btctl scan 10` on the real laptop NOT yet run: after the first trial (and the
  boot back to good, which came up fine) the laptop stopped answering (ping: host down) for the
  rest of the lane. Next step, one `lock.sh laptop` hold: deploy main, push
  userland/target/x86_64-unknown-none/release/{btd,btctl,bthid} to /root/bt (if the root image
  still lacks them), `btd &`, `btctl show`, `btctl scan 10`; firmware is already in
  /lib/firmware/intel on the stick. Script: $SP/lapbt-laptop2.sh.
- Regress: x86_64 + aarch64 full regress on the box: only pstest failed (the kthread-exit
  bug above, fixed by 0-thread-without-controller); pstest re-run PASS. rpi5 feature build not
  re-checked (hcidev.rs now builds on all targets; the rpi5-only pump is cfg-gated).
- SCO (isochronous interface 1) not supported; stalled-endpoint CLEAR_FEATURE(HALT) not sent.
