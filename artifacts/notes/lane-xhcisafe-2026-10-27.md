# lane xhcisafe (2026-10-27) - review-lapbt items 2-5

Status: DELIVERED (see evidence at the end).

## xHCI (drivers/usb/src/xhci)
- `recover_ep` failure now calls `quarantine_slot()`: Disable Slot for that slot only (the xHC stops its DMA,
  memory freed only on confirmed disable). The controller is halted (`die`) only if Disable Slot itself fails.
  Covers control()/bulk()/wait_td paths. btusb `poll` also quarantines an unrecoverable BT endpoint.
- btusb `InPipe::harvest`: an error completion no longer drops good completions behind it (first error is kept).
- `recoveries` resets to 0 after a harvest that returned data with no pending failure.

## Bluetooth (drivers/src/bt)
- `hcidev::claim()` (first transport wins /dev/hci0); used by boot.rs (UART) and usb.rs. `hcidev::is_open()`.
- `BtUsb::take_resets()`: set when an IN pipe was recovered; pump and set-up Link reset that reassembler.
- btintel: secure-send result checked/cleared before each next fragment, final wait uses the last one;
  DDC failures only warn (partial records counted); one reset-to-bootloader (0xfc01 {1,1,1,0,0..}) + retry when
  the download fails, and a final reset to bootloader on the second failure. New Link::bootloader().
- btusb pump: 100 ms sleep while /dev/hci0 is closed, 20 ms when open and idle (was 4 ms).
- Host tests (drivers/bt-tests): 41 pass, incl. new retry-after-reject test and bootloader-reset count.
