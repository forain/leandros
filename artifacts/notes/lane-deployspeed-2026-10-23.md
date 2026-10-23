# lane/deployspeed — 2026-10-23 (L6.8 deploy-files speed, L6.4 TLS over Wi-Fi)

## L6.8 "deploy-files ~0.2 MB/s" — PARTIAL (cause found; one driver bug fixed; rest is the radio link)

### Where the time goes (good kernel cf0514ad, Anvara 5G ch 40, -77 dBm, no Ethernet cable: eth0 `link: down`)

| path | result |
|---|---|
| guest `dd /dev/zero 50 MB \| cat` | 22 ms |
| guest dd 50 MB to /tmp (tmpfs) | 43 ms |
| guest dd 50 MB to /root (f2fs, stick) + sync | 0.84 s (~60 MB/s) |
| `remote.py push` 20 MB Mac -> guest /tmp (sftp) | 21.5 s, **0.93 MB/s** |
| raw `ssh 'cat > /dev/null' < 20 MB` | 17.6 s, 1.14 MB/s |
| `remote.py pull` 20 MB guest -> Mac | 142.6 s, **0.14 MB/s** (the reported "0.2 MB/s") |
| raw `ssh 'head -c 5 MB'` guest -> Mac | 32.3 s, 0.155 MB/s |
| wifibench 4 MB | down 9.26 Mbit/s, up **0.90** Mbit/s; tx_retry 2.08/frame, tx_fail 1.41 % |

remote.py / sftp / russh add nothing measurable: the raw ssh stream equals wifibench, and
f2fs/tmpfs are 50-1000x faster than the link. Guest TCP is not wasting airtime either: over a 3 MB
upload `stack_tx` sent 2155 packets for 3.17 MB (5 % overhead), 8 fast retransmits, 0 RTO. The
radio is: 109 frames/s, ~9 ms per frame, rate control at 6M legacy / MCS0-1/40.

### The driver bug

TX retry histogram (`tx_retries ok`, deliveries by retry count) since boot:
`4378 15295 909 4506 295 1238 91 449 21 194 ...` — deliveries peak at **odd** retry counts.
In STBC mode (default since wifirate2) the legacy part of the retry chain alternates antennas
but always started on antenna A, and STBC mode turns the antenna toggle/probe off. Antenna A is
the weak one at this spot, so nearly every legacy frame burned its first attempt on A.

Fix (`drivers/src/iwl_rs.rs` `legacy_ant_check`): count legacy attempts/deliveries per antenna
from the known chain; when the other antenna delivers >= 20 points more (>= 128 attempts each,
re-checked at most every 2 s) it starts the legacy chain. HT entries keep STBC on both antennas.
Host tests `stbc_legacy_chain_starts_on_the_antenna_that_delivers` (fails without the fix) and
`stbc_legacy_chain_keeps_antenna_when_equal` (no flapping at 50 vs 55 %); iwlwifi-tests 140/140.

### After (trial deployspeed-legant = main cbcdf0f0 + fix, same BSS Anvara 5G, -79 dBm)

| | before (good) | after (trial) |
|---|---|---|
| retry histogram | `4378 15295 909 4506 ...` | `13006 887 2361 248 767 ...` (peak at 0) |
| tx_retry / frame | 2.08 | 0.96 |
| tx_fail | 1.41 % | 0.62 % |
| wifibench up (3 runs) | 0.90 Mbit/s | 1.59 Mbit/s (1.98 / 1.59 / 1.45) |
| ssh guest -> Mac 5 MB | 0.155 MB/s | 0.18 MB/s |

rc flipped once (`toggles 1`, `ant B`). PROMOTED as good on the laptop.

### Left open
- The link itself: -77..-79 dBm on 5 GHz, rc lives at 6M legacy; MCS0-3 deliver 8-50 %.
  Laptop placement or an Ethernet cable is the real fix for deploy speed (e1000e is idle).
- Downlink varies 4-9 Mbit/s run to run (AP side, ap_retry ~20 %).
- After the reboot autoconnect picked 2.4 GHz "Anvara" (-74 dBm) over "Anvara 5G" (-77..-79):
  there 4.4 Mbit/s up / 3.8 down. Which band is better here is not settled.
- `netctl connect SSID --bssid` reports `unsupported-security` / `no-secret` for a saved
  WPA2/WPA3 profile; `connect --uuid` works. Repro: `netctl connect "Anvara 5G" --bssid <mac>`.

## L6.4 TLS "Decryption failed" over Wi-Fi

VERIFIED (no TLS error) on trial/good deployspeed-legant, Anvara 5G (-79 dBm, wifi0 only).
The guest has no curl; git over HTTPS (git 2.47.3, TLS) is the client: `git clone --depth 1
https://github.com/torvalds/linux /tmp/lx` twice, ~260 MB pack each (245 s and 144 s, ~1-1.8 MB/s).
Both downloads completed without any TLS / "decryption failed" / "bad record mac" error, ~520 MB in
total (more than the 5 x 100 MB asked for).

NEW, not TLS: both clones then failed locally with
`fatal: packfile /tmp/lx/.git/objects/pack/pack-<id>.pack cannot be mapped: Invalid argument`
(mmap of the ~260 MB pack on tmpfs returns EINVAL), followed by `remote did not send all
necessary objects`. Repro (laptop, any network): `git clone --depth 1
https://github.com/torvalds/linux /tmp/lx`.
