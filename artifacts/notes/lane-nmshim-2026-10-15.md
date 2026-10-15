# lane/nmshim (L4) — NetworkManager for the COSMIC network applet (2026-10-15)

Spec: the netmgr spec of 2026-10-15 (lane L4, "Shim hard rules"), design B §a/§b/appendix A,
design A §4. Based on main 8cf70b02 + lane/ncabi (b8da6ad5, fast-forwarded: lib/netctl-abi,
docs/servers/netctl.md). WPA3 deferred: SAE-only APs hidden, transition APs shown as PSK.

## What landed

- `leandros-sysbus networkmanager` (`ports/sysbus/src/nm/`). It is a stateless façade over
  `/dev/netctl`, and the kernel is the source of truth.
  - `backend.rs`: the Backend trait, the A/B profile-slot store (read-modify-write with seq+1
    into the other slot), and a null backend.
  - `netctl.rs`: the real backend. It opens O_RDWR, falls back to O_RDONLY (read-only), runs
    ioctls, and a reader thread reassembles the 64-byte records.
  - `mock.rs`: a scripted kernel.
  - `model.rs`: the NM model derived from the netctl records.
  - `engine.rs`: one task owns the backend; the D-Bus sync lives here.
  - `settings.rs`: NM dicts ↔ profiles.
  - `dbus.rs`: the zbus interfaces.
- Objects: NetworkManager root, Settings + Settings.Connection, Device (+Wired/+Wireless),
  AccessPoint, Connection.Active, IP4Config, AgentManager, and an ObjectManager at
  /org/freedesktop.
- PropertiesChanged comes from one mechanism. After every turn the engine calls GetAll on each
  exported interface and diffs it against what it last published, so no mutation can forget
  its signal (zbus proxies cache properties).
  - Explicit signals are queued by the model: Device/AC `StateChanged`,
    `AccessPointAdded/Removed`, `DeviceAdded/Removed`, NM `StateChanged`, `NewConnection`,
    `ConnectionRemoved`, `Updated`, `Removed`.
  - Each turn emits in a fixed order: export new objects, then the diffs, then the signals,
    then unexport. A returned path is therefore always readable, and a signal never names a
    path that does not exist yet.
- Hard rules, each checked by nmtest or the probe:
  - Every interface answers GetAll. `Wwan*` and `WirelessHardwareEnabled` exist.
  - APs linger 30 s after delisting and are pinned while they are an AC's SpecificObject. ACs
    linger 10 s.
  - HwAddress is always a MAC.
  - AC `Id` is the SSID (the profile id). The shim makes the uuids.
  - `Update` never errors: a read-only user gets an unsaved overlay, and Activate on it is
    PermissionDenied.
  - `RequestScan` always succeeds. The first scan holds its reply (≤ 6 s) until SCAN_DONE.
  - `LastScan` is never -1 (it is 0 before the first scan).
  - Connectivity is never 2.
  - Strength has a hysteresis of 5.
  - A wrong key gives device 120 reason 7 (held ≥ 0.5 s), then AC 4 reason 9.
  - Synthetic "Wired connection 1" (uuid `derive_uuid(WIRED, perm_mac)`) and a wired AC.
  - Agents are accepted and ignored.
  - AddAndActivate dedupes by SSID, because nmrs re-adds the profile on every connect that
    has a password.
- Backend selection, in order: `LEANDROS_NM_BACKEND=mock|netctl|none`; `LEANDROS_NM_MOCK`;
  `/etc/leandros/nm-mock` (empty file = built-in scenario, otherwise a scenario file);
  `nmshim.mock` on the kernel command line; `/dev/netctl`; the null backend (no devices,
  read-only).
- Mock scenario:
  - eth0 bound to 10.0.2.15/24.
  - SimHome-WPA2 (2 BSSIDs), SimMixed (transition), SimCafe-WPA3 (SAE-only, hidden),
    SimOpen, and SimFlaky (missing every third scan).
  - Timings follow the real driver: connect ≈ 0.9 s plus DHCP 0.5 s; a wrong key fails at
    2.0 s; disconnect takes 0.25 s; scans take 1.2 s and are coalesced within 10 s.
  - Profiles go through real slot files in `/tmp/leandros-nm-mock-<uid>`, using the
    netctl-abi serializer and parser.
  - `LEANDROS_NM_MOCK_RO=1` simulates a user outside netdev.
- Packaging:
  - `ports/dbus/session-pkg/services/org.freedesktop.NetworkManager.service`.
  - The mkfs freshness check for leandros-sysbus now also watches `lib/netctl-abi`.
  - `ports/cosmic-applet-network/` holds build.sh (the m6 recipe, pinned cosmic-applets
    e544cb5, nmrs 3.4.0) and the .desktop. mkfs stages the binary and the .desktop together
    or neither, and the build-all `stage_dbus_session` step builds it.
  - `userland/nmtest` (nmrs 3.4.0) is staged as /bin/nmtest and built by build-all.
  - Five NetworkManager checks were added to `leandros-sysbus probe`.
  - `.claude/skills/run-leandros/nmsession.py` is the applet screenshot driver.

## Results

nmtest = `/usr/bin/dbus-run-session -- /bin/nmtest --spawn` (mock), from the root serial
shell. Note: `dbus-run-session` is not on the root PATH there, so give the full path.

| | host (macOS busd) | aarch64 (HVF) | x86_64 (TCG) |
|---|---|---|---|
| nmtest (full, with a 25 s snapshot storm) | 19/19 | 19/19 | 19/19 |
| nmtest --ro | 6/6 | 6/6 | 6/6 |
| sysbus probe, mock via busd activation | NM 5/5 | 19/19 | – |
| sysbus probe, no /dev/netctl (null backend) | NM 5/5 | – | 19/19 |
| applet popup (virgl) | – | wired row, AP list, prompt, connected | wired row, AP list, prompt, wrong key |

- The wrong key, seen from nmrs: `AuthFailed` after ~4.0 s (2 s of that is nmrs' fixed scan
  wait). The device signals were (40,30,0) → (50,40,0) → (120,50,7).
- The right key: connected in ~3.4 s, with exactly one saved profile after the wrong attempt
  plus the right one (the dedupe works).
- The storm: 147 (aarch64) / 82 (x86_64 TCG) back-to-back snapshots with SimFlaky appearing
  and disappearing; 0 failures.

Screenshots are in `~/code/leandros-artifacts/nmshim-2026-10-15/{aarch64,x86_64}/`. The
Network applet is the rightmost panel icon (a wired-port symbol with eth0 up).

- aarch64 01–04: the applet with "Wired connection 1 · 10.0.2.15 · Connected 1 Gbps"; the
  visible networks (SimHome-WPA2, SimMixed, SimOpen, SimFlaky; no SimCafe-WPA3); the password
  prompt for SimHome-WPA2; and after Connect, "SimHome-WPA2 · 192.168.77.11 · Connected".
- x86_64 01–04: the same views, then a wrong key: "Unable to connect to network / Make sure
  … the password is correct". The profile is kept, as NM does.

Build: `./scripts/build-all.sh` (release, both arches) is green.

## Refuted / notable

- **The shim never showed up in a process list.** The first nmsession.py run reported "shim
  NOT ready" while the shim was serving. LeandrOS `/proc/<pid>/cmdline` shows only argv[0],
  so `networkmanager` is invisible there. The script now waits for the mock's profile
  directory instead.
- **No eager start is needed.** Busd activation from the applet's first snapshot GetAll is
  enough; nothing is started from start-cosmic-leandros. The shim requests the name only
  after the first kernel snapshot is applied, or after 3 s.
- **nmrs matches saved profiles by `connection.id == ssid`, not by `802-11-wireless.ssid`.**
  Profile ids therefore default to the SSID.
- **nmrs' StateReason numbering is its own** (7 means "SupplicantDisconnected" there, not
  NO_SECRETS). The shim emits real NM codes as the spec requires; nmrs maps both 7 and AC
  reason 9 to `AuthFailed`.

## For the other lanes (ABI requests — none blocking)

1. Please document that **attempt ids increase monotonically** within a boot. Today the doc
   only says "unique". The shim relies on it to ignore a stale WIFI_STATE of an older attempt
   that arrives before the new one's CONNECTING.
2. The shim depends on these doc guarantees: FAILED is always followed by DISCONNECTED or
   CONNECTING; DISCONNECT on an already-idle interface returns 0 with no event; a superseded
   attempt ends `FAILED/SUPERSEDED`. The shim treats SUPERSEDED as a non-failure: no 120 is
   shown.
3. The shim reads events with a **blocking read on a dup of the same open** while the engine
   thread issues ioctls on the original fd. L2: please keep that working, with one queue per
   open description, not per fd.
4. Wired activation state comes from `NcIface.flags` ADMIN_UP|CARRIER plus `NcIpv4.state`.
   L1/L2: please emit IFACE_CHANGED on admin up and down.

## Remaining gaps

- **The real `/dev/netctl` backend is untested** (L2/L3 are not merged). It compiles against
  the frozen ABI, and the fallback to the null backend was checked in-guest. The first thing
  to do after L2/L3 merge is to boot with `LEANDROS_WIFISIM=1` (L3) and run
  `nmtest --spawn` without `LEANDROS_NM_BACKEND=mock`. That needs a nmtest mode for
  wifi_sim's SSIDs and keys.
- The polkit netdev rule (design B §c) is not done.
- Agent GetSecrets (cosmic-settings' Wi-Fi page) is phase 2.
- IP6Config is "/".
- Hidden-SSID APs are not exported.
- There is no inter-process lock on the profile slots (two netdev shims writing at the same
  time would race on seq).
- nmtest is not in `runtests.py --suite regress` yet. The existing regress suite was not
  rerun in this lane; only additions were touched outside ports/sysbus.
- The mock is chosen by a file in /etc, so a test image must not ship `/etc/leandros/nm-mock`.
  mkfs does not create it.
