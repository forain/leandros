# lane/netwatch — 2026-10-17

Kernel cmdline watchdog so a trial kernel that breaks the only (Wi-Fi) network
path reboots itself back to the good boot entry.

- `netwatch=<s>` (default off): kernel thread (`kernel/src/netwatch.rs`, started
  in `init.rs` right after `net_daemon`) checks once a second; if no NIC has an
  IPv4 address (`net_server::any_ipv4_address()`, = any `IFRT[i].cfg` set, i.e.
  the same state `[NET] DHCP configured` records) `<s>` seconds after boot, it
  prints `[netwatch] no network address after N s — rebooting to the fallback
  entry` and calls `power::restart_now()` (= the reboot(2) restart path:
  `shutdown_prepare` sync_all + f2fs clean unmount, `machine_prepare` klog
  `flush_final` + disk cache flush, device quiesce from lane rebootq). The line
  is printed before the final klog flush, so it is in the previous boot's log
  (`espctl klog` / `remote.py logs`) on the next boot.
- `netwatch.loss=<s>`: after an address was seen once, all NICs without an
  address continuously for `<s>` s -> same reboot (message "network address
  lost for N s"). The address is dropped by the net daemon after a 4 s carrier
  grace, so this tracks carrier loss too. Not exercised in QEMU (no cheap way
  to hold a link down for the whole window in the test) — code path is the same
  as the boot deadline.
- `scripts/remote.py deploy-kernel --netwatch S`: appends `netwatch=S` to the
  trial entry's cmdline only (base = `--cmdline` if given, else the good
  entry's `good_cmdline` from `espctl status`); the good entry is untouched.
- Test: `.claude/skills/run-leandros/netwatchtest.py <arch>`; driver.py gained
  `LEANDROS_NIC=none` (no NIC device and `-nic none`; QEMU otherwise adds a
  default NIC). nonic boot must show the line and reset in the same QEMU;
  with-NIC boot must stay up past 40 s.

Caveat: if the machine has no address because DHCP is merely slow (> S), it
reboots; pick S generously (60-120 s on Wi-Fi). A trial kernel that never
reaches the network but is otherwise fine will also fall back by design.
