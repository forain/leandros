# lane/psklog — 2026-10-24 — Wi-Fi PSK in the kernel log (openbugs L7.1, P0 security)

Status: FIXED (QEMU-verified; the laptop was not touched).

## Root cause
The Wi-Fi code itself was already clean: `wifi_api::Secret` has a redacting
Debug, iwlwifi logs "passphrase not logged", wifi_sim/wifi_mgr/netctl never
print the PSK. The leak was the generic **kernel command-line echo**:

- `kernel/src/init.rs` printed `[BLK] kernel cmdline: <whole line>` on every boot
- `kernel/src/main.rs` printed `[early] cmdline: <whole line>` (non-Limine boots)

`wifi.ssid=`/`wifi.psk=` is a documented way to pass credentials, and every
console byte is copied into the persistent klog partition on the boot disk, so
the passphrase landed in plaintext in klog (and in every `espctl klog` /
`remote.py logs` copy taken off the machine). Separately, `/proc/cmdline`
returned the full line to every user.

## Fix
- New `servers/vfs/src/cmdline_redact.rs` (no_std, shared by kernel + VFS):
  a knob is secret when the last dot component of its key, lower-cased, is
  `psk|pmk|key|pass|passwd|password|passphrase|secret|token` or ends with
  `psk|passwd|password|passphrase|secret|token` (`wifi.psk` yes; `keymap`,
  `iommu.passthrough` no). Its value becomes `<redacted len=N>` — no value
  bytes, length kept so a too-short passphrase is still diagnosable.
- Both kernel cmdline echoes go through it unconditionally.
- `/proc/cmdline`: **redacted for non-root (fsuid != 0), real line for root.**
  Why not root-only: Linux makes it world-readable and tools read it as users;
  redaction keeps that working. Why not redact for root too: `kexec
  --reuse-cmdline` (root) must hand the next kernel the real PSK; init/espctl
  read non-secret knobs only. Root can read the ESP's limine.conf anyway, so
  hiding it from root buys nothing.
- dmesg/klog read permission: there is no in-memory dmesg/syslog(2) reader;
  the log is only reachable through the raw boot-disk log partition
  (`espctl klog`), i.e. raw block access. With the echo redacted the log no
  longer contains the value at all, so `dmesg_restrict`-style gating was not
  added (left as a hardening idea, not needed for this bug).

Other credential sites checked, no leak found: sshd (logs "accepted/failed
password for <user>", never the password), login/greeter/pam-leandros/sudo
(no password logging), nmtest already asserts no `psk` in NM GetSettings.
Not redacted (test-only by design): `wifisim.aps=` carries the simulated APs'
fake passphrases.

## Guard
`.claude/skills/run-leandros/psklogtest.py` (`runtests.py aarch64 <tag> psklog`):
boots with `wifi.ssid=PskLeakNet wifi.psk=testpsk-0123456789` (fake value) and
asserts the value is absent from the boot serial log (= what klog records),
from `espctl klog`, and from `/proc/cmdline` read as `leandro` (uid 1000,
must show `wifi.psk=<redacted len=18>`); root's /proc/cmdline still has it
(control).
- main acc28b68 images: **1/4** — proc_user FAIL, serial FAIL (3 lines, first
  `[BLK] kernel cmdline: ... wifi.psk=<FAKE>`), klog probe FAIL (probe bug, fixed)
- lane: **4/4** aarch64. (QEMU has no klog partition, so the klog row only
  shows 0 hits; the serial row covers the same bytes.)

## Open
- **The laptop's real PSK must be considered exposed**: it is in plaintext in
  the stick's klog partition (old boots' slots survive until overwritten) and in
  every klog/serial copy pulled to the Mac by earlier sessions' tool logs
  (`remote.py logs` outputs, /tmp and artifacts/ copies). Rotate the Wi-Fi
  passphrase; optionally delete old klog-*.txt copies.
- Optional hardening: gate raw log-partition reads / a future dmesg on root.
