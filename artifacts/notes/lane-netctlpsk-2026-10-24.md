# lane/netctlpsk (regression of cmdlinepsk a0fdf2c5) - 2026-10-24

## Regression
cmdlinepsk scrubbed `wifi.psk=` from the stored cmdline and parked it in `drivers::cmdline_secrets`
(take-once, consumed by iwlwifi). netctl's legacy profile still read `wifi.psk=` from the cmdline,
so it got an empty PSK: netroamtest `netroam_legacy_auto` + 9 dependents failed (baseline on main
6c640bf5, aarch64: fails=10, 810 s).

## Fix
netctl and iwlwifi are both in-kernel, so no ioctl is needed: `cmdline_secrets::peek()` (non-wiping
copy) is used by both consumers (they start in no fixed order, so take-once cannot serve both).
`/etc/leandros/wifi.conf` still wins in both. The secret never leaves the kernel; /proc/cmdline
and klog stay redacted.

## Evidence
See commit message / final report for test results.
