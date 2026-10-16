# lane/netint (L5) — network management end to end (2026-10-15)

Integration lane after the five network lanes merged into local main
`be405d0c` (ncabi, multiif, netctl, iwlctl, nmshim). Branch `lane/netint`.
Spec: netmgr-spec.md (2026-10-15). Release builds only; Mac: aarch64 HVF,
x86_64 TCG. Run ids `netint-a64`, `netint-x86` (+ `-nmreal`, `-sess`, `-ro`).

## Commits

| commit | what |
|---|---|
| `c17a0706` | driver.py `ssh-target` / remote.py `LEANDROS_REMOTE=qemu`: take eth0's `DHCP configured, address: X (eth0)` line, not the last one (follow-up f) |
| `3e8ab0c1` | ports/greetd/README.md: supplementary groups are real since fadbeb3b (follow-up e) |
| `e63dd676` | net, netctl: **wired admin down/up**, `features::WIRED_ADMIN`; CONNECT ids masked < 2^31; netctltest `wired_admin_*`; `netctl up IF` (follow-up c) |
| `eaeda61c` | netctl docs: dir 0770, zero-length slot = absent, attempt ids, FIONREAD, per-open-description queue, boot gen ≥ 1 (follow-ups a, b, d) |
| `7be21e74` | `nmtest --real` (+ `--after-reboot`, `--ro`, `--as USER [--no-netdev] [-- CMD]`, `--user-bus`), `nmrealtest.py`, runtests `regress` += mock nmtest, `--suite net-e2e`, nmsession `--real`/`--no-netdev` (goal 2, follow-up g) |
| `f039c40b` | driver `LEANDROS_ROOT_IMAGE`; nmrealtest/nmsession boot clones of the ROOT image too (bug found here, below) |
| `e55526eb` | nm shim: re-announce `WirelessEnabled` when a set is refused (bug found here, below) |
| (this note) | notes + consolidated laptop checklist |

## Results (final image = HEAD before this note; release; both arches)

| | aarch64 (HVF) | x86_64 (TCG) |
|---|---|---|
| `runtests --suite regress` (24 steps: 21 suites incl. netctltest 45 cases, mock `nmtest --quick`, sshsmoke, wifisimtest) | 24/24 PASS | 24/24 PASS |
| regress again on the final image | 24/24 PASS | 24/24 PASS |
| `--suite net-e2e`: netctltest | PASS (45/45) | PASS (45/45) |
| — mock `nmtest --spawn` (full, 25 s storm) | 19/19 | 19/19 |
| — mock `nmtest --spawn --ro` | 6/6 | 6/6 |
| — `nmrealtest.py` (real stack, own boot) | PASS 8/8 checks | PASS 8/8 checks |
| —— nmtest `--real` as leandro (netdev) | 19/19 | 19/19 |
| —— nmtest `--real --ro` as leandro without netdev | 6/6 | 6/6 |
| —— reboot: kernel loads slots, autoconnects before login | PASS (gen 1, 1 loaded, slot b) | PASS |
| —— nmtest `--real --after-reboot` | 2/2 | 2/2 |
| COSMIC session, `nmtest --as leandro --user-bus --real` (the session's busd-activated shim) | 19/19 | 19/19 |
| applet screenshots, real backend | AP list, prompt, wrong key, connected, cable pull/back, settings page | AP list, prompt, wrong key, connected, cable pull/back |
| applet as non-netdev user | sees list; connect refused (no crash, no profile); Wi-Fi switch snaps back (after fix) | same, fix verified in the shipped build |
| mock can never activate by default (`mock_off`) | PASS | PASS |

Real-stack timings (nmrs side, so each includes nmrs' fixed 2 s post-scan
wait where it applies), aarch64 / x86_64:
wrong key → AuthFailed 3.0 / 3.3 s; right key → activated + DHCP 2.1 / 2.4 s
(lease 10.0.3.15 on wifi0); **disconnect 106–137 / 147 ms** (target < 10 s);
no autoconnect within 5 s after a user disconnect; saved-profile reconnect
2.1 / 2.5 s; refused association fails in 2.3 s (no hang); wired admin
down + DHCP back 32–317 ms (slirp); radio on → APs back 0.4 s; after a
reboot wifi0 is already up on SimHome when the shim starts.

What `nmrealtest` / nmtest `--real` checks (wifi_sim default APs): AP list
shows SimHome, SimCafe (5 GHz), SimMixed (transition, shown as PSK only),
SimOpen, SimFull; **SimWPA3 (SAE-only) and SimHidden are not shown**; wrong
key → nmrs `AuthFailed` + device `StateChanged(120, _, 7)`; right key →
AC Activated, Id = SSID, IPv4 10.0.3.x, exactly one saved profile, device
100; the profile is in `/etc/leandros/network/profiles.{a,b}` (readable by
a netdev member, never by others); no `psk` in GetSettings; disconnect →
30 < 10 s; saved reconnect without password; open network; SimFull
refused; wired Device.Disconnect → 30 then `connect_wired` → 100 (new
WIRED_ADMIN); radio off → no APs, device 20; radio on → APs back and
autoconnect; Connectivity never 2. `mock_off`: no `/etc/leandros/nm-mock`,
no `nmshim.mock` on the command line, no `LEANDROS_NM*` in the activation
`.service` file (the shim then opens /dev/netctl; without it, the null
backend, never the mock). Group `netdev:x:108:leandro`. Boot log
`[netctl] profiles gen 1: 0 loaded (source 0, status 1)` (gen ≥ 1 even with
nothing to load).

Screenshots: `/Users/forain/code/leandros-artifacts/netint-2026-10-15/`
- `aarch64/`: 01-applet (wired row), 02-aplist (real sim list), 03-password,
  04-wrongkey ("Unable to connect to network"), 05-retry, 06-settings-network
  (cosmic-settings Network page on the real shim), 07-connected (SimHome ·
  10.0.3.15 · Connected, wired still connected), 08-cable-pulled (wired row
  gone, panel icon switches to Wi-Fi, wifi0 default route), 09-cable-back.
- `x86_64/`: 01-applet, 02-aplist, 03-password, 04-wrongkey, 05-connected,
  06-cable-pulled, 07-cable-back.
- `aarch64-ro/`, `x86_64-ro/` (leandro removed from netdev): AP list, password
  prompt, refused connect ("Unable to connect", nothing saved, wifi0 untouched),
  Wi-Fi switch: `04-ro-wifi-toggle-before-fix-stuck-off.png` (the bug) and
  `04-ro-wifi-toggle-after-fix.png` / `x86_64-ro/03-ro-wifi-toggle.png`.

## Follow-ups closed

- **a.** `/etc/leandros/network` is `root:netdev 0770` (mkfs already did it);
  `netctl_abi::PROFILES_DIR` doc and netctl.md said 0750 → fixed.
- **b.** netctl.md now states: zero-length slot = absent (NO_FILE); attempt
  ids strictly increase within a boot, CONNECT ids < 2^31 (now enforced:
  the counter is masked with `!AUTO_ATTEMPT_BIT`), autoconnect ids have bit
  31 set, wired CONNECT = attempt 0; FIONREAD answers bytes queued; the event
  queue belongs to the open file description (dup/fork share it, each
  open(2) gets its own queue + snapshot).
- **c.** WIRED_ADMIN: `servers/net` `iface::set_admin(netif, up)` (exported as
  `set_iface_admin`); the daemon's `track_admin` applies it per pass — down:
  `deconfigure(.., "admin down")` at once (no carrier grace, TCP reset), the
  stack is not polled so DHCP sends nothing; up: DHCP restarts like a link
  change. `IfSnapshot.admin_up` → links.rs clears ADMIN_UP / oper DOWN /
  ipv4 NONE, so IFACE_CHANGED + IPV4 arrive through the iface hook.
  `links::FEATURES = WIRED_ADMIN`. netctltest `wired_admin_down`,
  `_down_again_ok`, `_up_rebinds` (1 ms down, 7 ms to a new lease on
  aarch64 slirp). The shim's wired Disconnect/Activate works (nmtest
  `--real` "wired: Device.Disconnect then connect_wired"). CLI:
  `netctl disconnect eth0` / `netctl up eth0`. Not persisted: every netif
  boots admin up.
- **d.** Confirmed in code and at runtime: `reload_profiles()` at boot always
  publishes (`PROFILES_GEN` starts at 1), legacy wifi.conf / `wifi.ssid=`
  merged by `profiles::load`; documented in netctl.md.
- **e.** greetd README fixed.
- **f.** driver.py `eth0_lease()` + remote.py: last lease line *of eth0*
  (a line without `(ifname)` from older kernels counts as eth0).
- **g.** regress gains the mock `nmtest --spawn --quick` (~20 s); the real
  stack is `--suite net-e2e` (netctltest + mock full + mock --ro +
  nmrealtest: ~1.5 min aarch64, ~3.5 min x86 TCG) so regress stays short.

## Bugs found and fixed

1. **Test clones shared the ROOT image.** `LEANDROS_DISK_IMAGE` replaces only
   the ESP image; `f2fs-data0-<arch>.img` stayed shared, so nmrealtest's saved
   SimHome profile landed in the build's root image (next boot of the build
   image autoconnected to SimHome). New `LEANDROS_ROOT_IMAGE`; nmrealtest and
   nmsession clone both images. (wifisimtest still clones only the ESP; its
   self-test does not write the slots, so it is harmless today.)
2. **Read-only user's Wi-Fi switch stuck off.** The COSMIC applet flips the
   switch optimistically; the shim refused (AccessDenied) and, nothing having
   changed, published no PropertiesChanged, so the switch stayed off while the
   radio stayed on. The setter now re-announces `WirelessEnabled` on any
   refused/failed set.
3. `nmtest --as` first inherited root's `XDG_RUNTIME_DIR=/run/user/0`:
   dbus-run-session could not write its readiness file. `--as` now creates
   `/run/user/<uid>` (0700, owned by the user) like a login, sets
   `XDG_RUNTIME_DIR`, drops inherited bus addresses, and chdirs to $HOME.

## Still open

- nmrs reports a refused association (SimFull, status 17) as
  `connection failed: unknown`; the applet shows the generic "Unable to
  connect". The shim could map ASSOC_REJECTED to a more specific NM reason.
- The applet gives a read-only user the generic "Unable to connect … password
  is correct" text instead of "not permitted" (upstream applet has no
  permission UI; polkit netdev rule from design B §c is still not done).
- After a wrong key NM-style keeps the profile with the wrong secret; the
  applet's "Connect" retry reuses it (fails again). The user has to Forget
  (Settings) to be prompted again — same as real NM, but worth knowing.
- virtio-net reports speed 0, so the wired row shows no "1 Gbps" (mock did).
- Laptop: everything in the checklist below; iwlwifi `mgr=1` and the
  soft-teardown firmware unknowns are untested on hardware.
- Inherited from the lanes and not touched here: no inter-process lock on the
  profile slots; Agent GetSecrets (cosmic-settings Wi-Fi page) phase 2;
  IP6Config "/"; hidden SSIDs not exported; UDP INADDR_ANY on one stack;
  netctl blocked read(2) parks with a 10 ms deadline; `/proc/<pid>/status`
  512-byte buffer.
- Not pushed (lane rule).

## LAPTOP CHECKLIST (EliteBook 840 G5, stick) — one list for multiif + netctl + iwlctl + nmshim

Tools: `laptop-deploy-kernel` (kernel + cmdline A/B, never bricks the stick),
then `laptop-remote-debug` (`remote.py run/pull/dmesg`). Start from a stick
with `/etc/leandros/wifi.conf` and EMPTY profile slots, as today. Never write
the passphrase into logs, notes or commits (`netctl connect` takes it on
stdin/args — clear shell history; the shim never logs it).

### A. Safety first: `iwlwifi.mgr=0` (the default cmdline), nothing may change

1. Boot with the cable IN. Console: `[NET] netif eth0 = e1000e`,
   `[NET] netif wifi0 = iwlwifi`; `/proc/net/nic` has `[eth0]` and `[wifi0]`,
   no `manager:` line. `/proc/net/route`: eth0 metric 100 first, wifi0 600.
2. `dmesg | grep iwlwifi`: "credentials from /etc/leandros/wifi.conf" →
   "4-way handshake complete" → "wlan: link up ... (wifi0)". Two lease lines:
   `DHCP configured, address: X (eth0)` and `... Y (wifi0)`.
3. SSH answers on BOTH addresses before any login.
   `remote.py discover` finds it; `LEANDROS_REMOTE=qemu` targets now take
   eth0's lease line, not the last one (fixed in netint).
4. `[netctl] profiles gen 1: 1 loaded (source 0, status 1)` on the console
   (gen ≥ 1, the legacy wifi.conf network merged, flag LEGACY in
   `netctl profiles`). `ls -la /etc/leandros/network`: dir `root netdev
   drwxrwx---`, slots `-rw-rw----`.
5. `netctl status` as root and as leandro (netdev): eth0 up + bound + default
   route, wifi0 listed. As a non-member (e.g. a fresh user): `netctl connect`
   → EACCES on open, read-only view works.
6. e1000e: `rx_irq on` still in the netd line.
7. `dmesg | grep -i 'firmware failure\|assert'` is empty.

### B. Wired failover (still mgr=0)

8. Pull the cable: `eth0: carrier down`; new flows leave via wifi0 at once
   (`netcheck --via 1.1.1.1 80` says wifi0); after ~4 s `eth0: address ...
   removed`; SSH via the Wi-Fi address keeps working; `/etc/resolv.conf`
   lists the Wi-Fi DNS first. Plug back: lease again, default route back on
   eth0, held SSH over wifi0 still alive.
9. Boot WITHOUT the cable: everything via wifi0, `netcheck` says
   `nic| checking wifi0`; anything dialled before a lease gets ENETUNREACH
   at once (not a blackholed SYN).
10. Wired admin (new, `features::WIRED_ADMIN`): `netctl disconnect eth0` →
    console `[NET] eth0: admin down` + `address ... removed (admin down)`,
    traffic moves to wifi0; `netctl up eth0` → `admin up` and a new
    eth0 lease within a few seconds. (Do this over the Wi-Fi SSH address.)

### C. The connection manager: `iwlwifi.mgr=1` (cmdline A/B entry)

11. Boot: "manager running", "autoconnect: '<home>'", "connected to '<home>'
    in N ms". **SSH reachable before login.** Record N.
12. `/proc/net/wifi`: AP list with JOIN/AGE columns; `/proc/net/nic`:
    `manager: wifi_mgr`, `wifi_state: connected`.
13. `netctl scan` while connected: a COALESCED SCAN_DONE, no link drop
    (scan-while-associated is off unless `iwlwifi.scanconn=1`).
14. `netctl disconnect wifi0`: "disconnected … on request in N ms", N < 2000
    (target < 100), then "left the BSS without a firmware restart (M ms)".
    "teardown failed … restarting" instead → record the error (the
    soft-teardown commands are the main firmware unknown). No autoconnect
    afterwards until a connect or reload.
15. `netctl connect <home>` (right key): connected; time it — this is the
    reconnect after a soft teardown.
16. `netctl connect <home>` with a WRONG key: FAILED wrong-key within ~6 s
    ("the AP ended the handshake (reason 15)" or "timed out after message
    2"). Then the right key again.
17. `netctl radio off`: RADIO_OFF, AP list empty. `netctl radio on`:
    DISCONNECTED → scan → autoconnect back. SSH back.
18. Save a profile (`netctl save <home>` or via the applet, step 22), reboot:
    the kernel loads `profiles.{a,b}` at boot (`[netctl] profiles gen 1: 2
    loaded (source 1|2, status 0)`) and autoconnects BEFORE login; then
    remove the slots' content (`netctl forget`) and reboot: the legacy
    wifi.conf fallback still connects.
19. Optional: `iwlwifi.scanconn=1` (fresh scan while connected, link stays
    up, no asserts); `iwlwifi.softdisc=0` (disconnect < 2 s still, next
    connect slower: firmware restart).
20. Always: `dmesg | grep -i 'firmware failure\|assert'` empty.

### D. The COSMIC network applet on real Wi-Fi (mgr=1, logged in as leandro)

21. Panel: the Network applet (rightmost) shows the wired row ("Wired
    connection 1 · <eth0 address> · Connected 1 Gbps" with the cable in) and
    the Wi-Fi list (real SSIDs; WPA3-only networks hidden, transition
    networks shown; no hidden-SSID entries).
22. Click your home network → password prompt → right key → "Connected"
    with the wifi0 address within ~5 s. `ls -la /etc/leandros/network`: one
    slot rewritten (seq + 1), still `root:netdev 0660`.
23. Disconnect from the applet → wifi0 disconnected in < 2 s (nmrs gives up
    at 10 s); it does not autoconnect back by itself.
24. Click it again (saved, no prompt) → connected.
25. Wrong key on another network (or after "Forget"): "Unable to connect to
    network / ... the password is correct" within ~6 s; the applet does not
    hang; the home network still reconnects afterwards.
26. Airplane / Wi-Fi toggle off → list empty, wifi0 radio off; on → list
    back, autoconnect.
27. Cable pull with the applet open: the wired row goes to disconnected,
    the Wi-Fi row stays connected and becomes the default route; plug back:
    wired row Connected again. The applet never blanks or crashes
    (`cat /run/user/1000/*.log` / session log has no applet panic).
28. Wired row "Disconnect" in the applet (admin down) → eth0 loses its
    address; "Connect" → new lease.
29. A user outside netdev (remove leandro from `netdev` in /etc/group,
    re-login): the applet lists networks, clicking one does NOT connect
    (no crash, no hang), the Wi-Fi toggle does nothing.
30. Reboot with the profile saved: wifi0 connects before login; after
    login the applet shows it connected without any click.
