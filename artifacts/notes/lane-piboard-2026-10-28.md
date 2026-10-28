# lane/piboard 2026-10-28 (design items 2 and 3): FIXED

- Fan: rp1_fan::tick(temp) from the pi5_board loop every 1 s; rp1_hw::fan_next = fan_state + 85 C override; knob fan=auto|off|0..4;
  /proc/thermal gains `fan: state=S level=L/255 rpm=R mode=auto|fixed`. Pi trial 7921e169ce8d: 40.2 C state 0 rpm 0;
  4x yes for 120 s: 52.9 C state 1 level 75/255 rpm 1046; log `[rp1fan] 50.1 C state 0->1 level 75/255`.
- bdaddr: bt.bdaddr=auto|dt|otp|keep|xx:xx:xx:xx:xx:xx. On the Pi the OTP/firmware MAC tag FAILS (`[otp] mac=none`), so the
  Ethernet MAC from the firmware cmdline smsc95xx.macaddr (2C:CF:67:D8:CA:1A) + 2 is the fallback (as brcmf does for Wi-Fi).
  Trial 262ea56be7db: `[bt] hci ... bdaddr=2c:cf:67:d8:ca:1c (set-from-cmdline-mac)`, `[btd] ready bdaddr=2C:CF:67:D8:CA:1C`.
  The 8BitDo bond to 43:45:C0:00:1F:AC is gone: re-pair once (or bt.bdaddr=keep).
- Host tests: rp1-tests fan_next edges, bcm2712-tests bt_from_eth incl. carry, bt-tests byte order and knob selection.
- Open: why GET_BOARD_MAC_ADDRESS fails on the Pi 5 firmware. One-line change in kernel/src/init.rs (rp1_fan::configure).
