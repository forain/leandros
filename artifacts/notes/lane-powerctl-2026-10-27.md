# lane/powerctl — 2026-10-27: power off / reboot as a regular user and from the greeter

## Verdict: FIXED (greeter) / VERIFIED + hardened (user session)

### Root causes
1. Greeter buttons did nothing: cosmic-greeter was built `--no-default-features`, which
   compiles out its `logind` feature — the Restart / Shut down dialog's confirm handler is
   `#[cfg(feature = "logind")]`, so confirming was a no-op. It also had no system bus at all
   (greeter-launch dropped DBUS_SYSTEM_BUS_ADDRESS because busd once never answered calls
   to unowned names; ports/busd/service-unknown-reply.patch fixed that since).
2. User session: the D-Bus path (cosmic-osd -> login1 PowerOff/Reboot -> init initctl,
   authorised by init when the requester descends from a local login) already worked in
   QEMU; there was no `systemctl`, and `poweroff`/`reboot` as non-root spoke to init
   directly rather than through login1.

### Changes
- ports/cosmic-wgpu/build-wgpu-clients.sh (+ artifacts/m6-session-bins/build-greeter.sh):
  greeter built with `--features logind`; also fixed the empty-array `set -u` abort under
  macOS bash 3.2. Rebuilt out-wgpu/ and out/cosmic-greeter-{aarch64,x86_64}
  (previous binaries kept as out/cosmic-greeter-*.pre-powerctl).
- userland/greeter-launch: execs `sh /usr/bin/dbus-run-session -- /bin/cosmic-greeter-login`
  with DBUS_SYSTEM_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus (the greeter account's
  0700 /run/user/990), so login1 is activated there as uid 990 under greetd.
- ports/sysbus: `leandros-sysbus power <PowerOff|Reboot|Halt|CanPowerOff|CanReboot|CanHalt>`
  (the systemctl/loginctl-style login1 client).
- userland/poweroff: non-root + a bus in the environment => exec the login1 client
  (Linux way: systemctl -> logind); otherwise init's socket as before. New `systemctl
  poweroff|reboot|halt` name (hardlink). reboot(2) stays root-only (kernel unchanged).
- Authorisation stays in init (root): requester must be root or descend from greetd /
  a console login (polkit allow_active). Reaching login1 already requires access to the
  user's 0700 runtime dir. Can* answer "yes" while init listens.

### Evidence (aarch64 HVF, .claude/skills/run-leandros/powerctltest.py, 3 boots)
- ssh as leandro: `reboot` and `systemctl poweroff` -> init "DENIED", "Access denied", VM up.
- greeter (uid 990, its own bus): CanReboot=yes; Reboot -> "reboot requested by pid 104
  (uid 990): authorised", rebooted.
- leandro session: CanPowerOff=yes; `/bin/reboot` via login1 -> "(uid 1000): authorised",
  rebooted; login1 PowerOff over D-Bus -> "power-off ... (uid 1000): authorised", QEMU exited.

### Open
- Greeter buttons were exercised through the greeter's bus, not by clicking; real-laptop
  click-through not done. If the user's session menu still fails on the laptop, capture
  cosmic-osd's log + the `[init] ... requested by` line (a DENIED line names the pid whose
  ancestry is not under greetd).

- Merge note: leandros-sysbus is staged into the SHARED m5-session-ship dir by every lane build; a concurrent lane build overwrote it once (power subcommand missing). After merge, main carries it.
