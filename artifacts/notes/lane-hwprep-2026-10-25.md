# lane/hwprep 2026-10-25 (openbugs #62, L11.6 prep)

## #62 wifi.psk on the ESP: FIXED
Tokens found (value never printed): `wifi.psk=` in the flashed `/leandros/factory.conf` cmdline, copied into the
`factory` entry and (as base cmdline) the `good` entry of `/limine.conf`: 2 in limine.conf, 1 in factory.conf.
Every slot cmdline = flashed base + extra, which is why good carried root=/i915 knobs twice.
Fix: new `espctl scrub-psk` (userland/espctl/src/main.rs) strips `wifi.psk=`/`iwlwifi.psk=` tokens from factory.conf
and limine.conf (shadow-safe commit_conf). Ran on the laptop: counts 2/1 -> 0/0, also 0 after the next
deploy-kernel (install derives from the scrubbed factory.conf). Wi-Fi still came up on the trial (reads /etc/leandros/wifi.conf).
Caveat: the factory kernel (a3f226cc) predates wifi.conf, so it no longer auto-joins Wi-Fi (fallback only).
Fresh sticks from write-usb.sh may still flash a psk cmdline: run `espctl scrub-psk` once after flashing.

## L11.6 prep
Trial kernel = main dd5e6533 + EV_STATS=true + evstat dev range 0..8 (local only). `[EVSTAT]` reaches klog (56 lines
in the first boot). Only event0 (keyboard) and event1 (pointer) exist; mouse = dev 1. See scratchpad hw/READY.md.
