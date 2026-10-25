# Lane linuxcmp2 — 2026-10-25: Linux vs LeandrOS Wi-Fi RX on the same laptop, AP, spot and window (L6.1)

**Verdict: REFUTED as our bug. It is on the AP/channel side.** Linux 6.12 iwlmvm with the same 8265-36 firmware gets
the same AP2 throughput as LeandrOS, or less, in the same 20-minute window.

## Method
- Commits: lane/linuxcmp 237a64b0 (`espctl oneshot`/`oneshot-restore`), plus ec3ac5f0 (tooling in scripts/linuxcmp/).
- Credentials: Linux refuses to mount LeandrOS f2fs (#46), so the initramfs carries `f2rd`. It is a static
  x86_64 read-only reader (scripts/linuxcmp/f2rd.c): checkpoint pick, journal overlay, NAT and dentry lookup, as
  scripts/fsck-f2fs.py does. It reads /etc/leandros/wifi.conf from the raw sda2 into shell variables. The PSK goes
  only into tmpfs /tmp/w.conf, which is removed at finish, and only its length is logged.
  Verified: md5 of /etc/passwd matches on the Mac and in Linux under QEMU, and the laptop log says `ssid=Anvara psk_len=8`.
- campaign.sh runs inside one laptop lock hold: ours o1, Linux L1, ours o2, Linux L2, ours o3, all between 04:24 and 04:47.
  Each round is AP2 6 MiB x3 and control 24 MiB x2, received by the laptop from the Mac (172.16.166.21).
  Linux uses wget from server.py. LeandrOS uses `ssh cat >/dev/null`, because the image has no wget/curl.

## Numbers (Mbit/s, laptop RX)
| BSSID | o1 04:24 | L1 04:29 | o2 04:32 | L2 04:40 | o3 04:44 |
|---|---|---|---|---|---|
| AP2 74:83:c2:ba:21:df ch6 | 4.26 3.40 4.33 | (16.6 invalid, rc=1) 5.22 6.58 | 2.49 1.36 2.28 | 1.52 1.83 1.40 | 3.62 1.46 2.95 |
| ctl 84:78:48:d8:7e:79 ch1 | 4.10 3.10 | 5.60, <4.47 (45 s timeout) | <1.65 (timeout) 2.16 | <4.47 x2 (timeouts) | lost (ssh dead) |

Both OSes move together with time (L1 and o1 good, then o2 and L2 bad). Linux is not systematically faster.
- Linux `iw station dump`: AP2 signal -76..-78 dBm, rx MCS2-4 (19.5-39 Mbit/s), tx MCS11 to MCS3, tx retries 245 / 939.
  ctl signal -73 dBm, rx MCS4-5, tx retries 819 / 623. power_save on.
- LeandrOS /proc/net/nic (o1): AP2 signal -73, rx retry 26 %, rx mix MCS2-4. ctl signal -70, rx retry 26 %, rx MCS2-4.
  The LeandrOS TX rate control stays SISO (MCS3s-MCS7s). Linux transmits 2-stream MCS11. That only affects ACK airtime
  in this test, so it is a possible later improvement, not the L6.1 cause.

## Notes
- The "19-21 Mbit/s other AP" from the rxap2 lane was Anvara 5G (8a:78:48:d8:7e:7a, ch40). The ctl BSSID here is the same
  router's 2.4 GHz radio, and it is as slow as AP2 on both OSes. The slowness is 2.4 GHz at this spot
  (-73..-78 dBm, a congested band with 13+ BSS), not AP2 specifically and not our driver.
- Rekeys: Anvara rekeyed its group key every 60 s for lane rekeychk during the whole window. A GTK rekey affects
  only group traffic plus one 2-frame handshake per minute, and both OSes ran under it. It cannot explain a 2x swing,
  and it does not bias the comparison.
- Open (new): in o3, right after `netctl connect --bssid 84:78:48:d8:7e:79`, ssh and ping to the laptop were dead for
  several minutes. They recovered by themselves (ping ok about 5 min later). This looks like the rxap2 unreachable episode. Repro: the o3 ctl step of ours.sh.
- Laptop safety: both Linux one-shots restored limine.conf before Wi-Fi (sha matched), and the laptop was back on
  good (armed=0) in 198 s and 264 s. The ESP /linuxcmp directory was removed each time.
