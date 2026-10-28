# lane/lapwifitx — 2026-10-28

Merged: BAR only for real Block Ack holes (d9b80497) and VHT TX rates (b89a8792, knob iwlwifi.vhttx, now default OFF).
Builds: x86_64 INTEL=1 and aarch64 release clean (0 `^error`); wifisimtest PASS on both arches.
Laptop (good 2d3e00b630f2, wifibench 3x16 MB, ch 1 2.4 GHz HT, -51..-57 dBm; the link was not VHT, so VHT TX is unexercised):
| kernel | down Mbit/s | up Mbit/s | ap_retry | tx_retry/frame | tx_fail |
|---|---|---|---|---|---|
| good | 56.8 | 37.6 | 16.9% | 0.09 | 0% |
| trial (BAR fix, default) | 62.4 | 38.8 | 11.5% | 0.04 | 0% |
| trial + vhttx=0 | 60.1 | 40.8 | 13.6% | 0.04 | 0% |
No regression; BAR fix halves tx_retry. Down/up differences are within run noise except retries.
vhttx default flipped to off (iwlwifi.vhttx=1 to enable) because it was never run on a 5 GHz VHT link.
Laptop returned to good 2d3e00b630f2; nothing promoted.
