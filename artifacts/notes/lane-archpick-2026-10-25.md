# lane/archpick 2026-10-25 (openbugs #78)

FIXED. wifisimtest (and netroamtest, nmrealtest, udppeertest, wifihtbench) chose the arch
from `driver._qemu_pid()` (whichever QEMU the run id saw) before LEANDROS_TEST_ARCH. Order is now
argv > LEANDROS_TEST_ARCH (set by runtests.host_run) > detected VM (only used to stop it) > aarch64.
Scripts that defaulted to "aarch64" with argv-only now fall back to LEANDROS_TEST_ARCH:
blanktest, blkfaulttest, blkhangtest, coreutils-test, nvmefaulttest, usbstalltest, usbunplugtest,
psklogtest, trialwatchtest, pagecache-crashtest, reboottest, f2fscptest, f2fscrashtest, f2fsfulltest,
f2fsgctest, f2fsorphantest.

Evidence: with an aarch64 VM up (run id archpick-bg), `runtests.py x86_64 host:wifisimtest.py`
booted x86_64 ("PASS 39/39 ... arch x86_64"); aarch64 run PASS 39/39, RC=0.
Open: on x86_64 (TCG, images copied from main target/) netctl_connect_ssid_bssid printed FAIL
("unsupported-security", L10.13 case) so that run's RC=1; not arch related, possibly a stale x86
initrd - re-check on a fresh x86 build.
