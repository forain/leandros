# lane/lapptpfix 2026-10-28/29

Regression: default touchpad=ps2 + i2c_hid=probe ran a 120 s plain I2C-HID input-read listen on CPD0
(plus _PS0/_INI scan); the Synaptics pad latched into I2C/HID mode (EC standby power, survives power
cycles), AUX2 went silent, touchpad dead (trackstick fine).

Fix (main 9001d8ab, not pushed):
- i2c_hid::init: default mode touches nothing (no scan, no probe, no listen); listen is opt-in via i2c_hid.listen=1.
- Auto: if no PS/2 port found a Synaptics pad, use the I2C PTP driver ("touchpad: ps2 pad silent, using i2c (CPD0)").
  touchpad=ps2! forces PS/2.
- iwlwifi.mgr defaults on (mgr=0 opts out).
- psmouse drop hook only set after setup_pad succeeds (unchanged).

Laptop: touchpad=i2c trial 092d21b3d016 fixed the pad; plain-cmdline trial 5d4d6b6cdfcd then showed
AUX2 Synaptics absolute again (the I2C driver's reset returned the pad to PS/2). Auto-i2c path not exercised there.
Tests: aarch64 regress PASS; x86_64 only known-red poll_timeout_wake_latency.
