# `/dev/netctl`: the network control protocol

`/dev/netctl` is the one control node for network interfaces and Wi-Fi. A client:

- reads interface, IPv4, Wi-Fi, scan and saved-profile state from it, and
- if it is allowed to, scans, connects, disconnects, switches the radio and reloads profiles.

Clients include the `netctl` CLI, the NetworkManager shim in `ports/sysbus` that the COSMIC network applet talks to, and tests.

**Status: frozen at ABI version 1 (2026-10-15, lane ncabi).** The byte-level definition is the `no_std` crate [`lib/netctl-abi`](../../lib/netctl-abi). The kernel and userland both use it, so the ABI cannot drift. This page specifies the semantics. Where this page and the crate disagree on a byte, the crate wins, and the disagreement is a documentation bug. Change the protocol only by appending (see [Versioning](#versioning)).

Implementation lanes:

| Lane | Part |
|---|---|
| L1 | The interface registry (`drivers/src/netdev.rs`) and one IP stack per interface (`servers/net`) |
| L2 | The device itself (`servers/net/src/netctl.rs`) |
| L3 | The Wi-Fi backends behind [`drivers/src/wifi_api.rs`](../../drivers/src/wifi_api.rs) |
| L4 | The NM shim |

## Overview

| Piece | Summary |
|---|---|
| Node | `/dev/netctl`, char **10:242**, `root:netdev 0664`. `netdev` is gid **108** |
| Commands | ioctls, type `'N'`. Seven read ioctls (any open) and five mutating ioctls (write-capable open only) |
| State | `read(2)` of fixed **64-byte** event records. Every open starts with a full snapshot, then deltas. `poll`/`epoll` give `POLLIN` |
| Overflow | Lossless. The queue is dropped, then `RESYNC` and a fresh snapshot follow |
| Profiles | `/etc/leandros/network/profiles.{a,b}`, A/B slots, `seq` + CRC-32, newest valid wins. Userland writes them, the kernel reads them |
| Interfaces | `lo` = ifindex 1. Wired NICs are `eth0`, `eth1`, …, and Wi-Fi is `wifi0`. Default route metric: wired 100, Wi-Fi 600 |
| Security | WPA2-PSK now. The AKM field already carries `PSK_SHA256` and `SAE`, so WPA3 needs no ABI change |

## Permission model

The kernel checks once, at `open(2)`, as Linux does for a device node:

- **`O_RDONLY`**: anyone. A read-only open can read events, use every `NETCTL_GET_*` ioctl and `NETCTL_REQUEST_SNAPSHOT`.
- **`O_WRONLY` / `O_RDWR`**: needs euid 0, or gid 108 as the egid or among the supplementary groups. Otherwise open fails with **`EACCES`**. This is the ordinary VFS mode check on the node's `root:netdev 0664` metadata (`dyn_dev_meta`).
- The device server records at `VFS_OPEN` whether the open is write-capable (access mode ≠ `O_RDONLY`).
  - A **mutating ioctl** (nr ≥ `0x20`: `SCAN`, `CONNECT`, `DISCONNECT`, `SET_RADIO`, `RELOAD_PROFILES`) on a read-only open answers **`EPERM`**.
  - This is enforced in the device server itself. It does not rely on the VFS refusing anything.
- Nothing is re-checked per call. A descriptor passed with `SCM_RIGHTS` or inherited by `fork` keeps the capability of its open. Removing a user from `netdev` takes effect at their next login.
- There is no broker. The NM shim runs as the session user and opens the node with that user's credentials, so it cannot launder authority. A `netdev` member who bypasses the shim and talks to the node directly is doing exactly what membership allows.
- `write(2)` is not part of the protocol and answers `EINVAL`.

`/etc/group` carries `netdev:x:108:leandro`. The greeter is not a member and gets the read-only view.

## Conventions

- **Layout.** Every struct is `#[repr(C)]` and made of integers and integer arrays. Every padding byte is an explicit `_pad`/`_rsv` field, which writers zero and readers ignore.
  - Values are native-endian. Both targets are little-endian.
  - Addresses are network byte order: `a.b.c.d` = `[a, b, c, d]`.
  - SSIDs are 0..32 raw bytes, not necessarily UTF-8.
  - Names are NUL-padded.
- **Enumerations** are `u8` or `u32` integers, listed in `netctl_abi::types` (`if_kind`, `akm`, `wifi_state`, `reason`, …). Readers must tolerate values they do not know.
- **Command structs start with `size: u32`**, which is the caller's `sizeof`.
  - The kernel reads the `u32` first, copies `min(size, its own)` bytes, and zero-fills the rest. Out fields are written back for the same length.
  - `size` smaller than the ABI-1 size of the struct answers `EINVAL`.
  - Each struct's `new()` sets `size`.
- **ioctl numbers** use Linux's generic `_IOC(dir, 'N', nr, size)` encoding. The kernel dispatches on type and nr only; the size and direction bits are informational.
- **Errors** are negative Linux errno values:

  | errno | When |
  |---|---|
  | `EPERM` | read-only open |
  | `ENOENT` | profile uuid not loaded |
  | `EFAULT` | bad pointer |
  | `ENODEV` | unknown ifindex, or the wrong kind of interface |
  | `EINVAL` | malformed struct or lengths |
  | `ENOTTY` | unknown ioctl |
  | `EOPNOTSUPP` | AKM or feature not available |
  | `ENETDOWN` | radio off or firmware not ready |
- **Time.** `time_ms`/`since_ms`/`last_seen_ms` are boot-time milliseconds (CLOCK_BOOTTIME). The 32-bit ones wrap after 49 days. `NcScanInfo.time_ms` is 64-bit.
- **ifindex 0** in a command means "the first Wi-Fi interface", or "all of them" where stated. In a query it means "all interfaces". In an event header it means a global event.

## ioctls

| Name | Encoding | Value | Arg | Open |
|---|---|---|---|---|
| `NETCTL_GET_VERSION` | `_IOR('N',0x00)` | `0x80204E00` | `NcVersion` (out) | any |
| `NETCTL_GET_IFACES` | `_IOWR('N',0x01)` | `0xC0204E01` | `NcQuery` → `NcIface[]` | any |
| `NETCTL_GET_IPV4` | `_IOWR('N',0x02)` | `0xC0204E02` | `NcQuery` → `NcIpv4[]` | any |
| `NETCTL_GET_WIFI` | `_IOWR('N',0x03)` | `0xC0204E03` | `NcQuery` → `NcWifiState[]` | any |
| `NETCTL_GET_BSS` | `_IOWR('N',0x04)` | `0xC0204E04` | `NcQuery` → `NcBss[]` | any |
| `NETCTL_GET_PROFILES` | `_IOWR('N',0x05)` | `0xC0204E05` | `NcQuery` → `NcProfileMeta[]` | any |
| `NETCTL_REQUEST_SNAPSHOT` | `_IO('N',0x06)` | `0x00004E06` | none | any |
| `NETCTL_SCAN` | `_IOW('N',0x20)` | `0x40A04E20` | `NcScan` | **W** |
| `NETCTL_CONNECT` | `_IOWR('N',0x21)` | `0xC0D04E21` | `NcConnect` (attempt out) | **W** |
| `NETCTL_DISCONNECT` | `_IOW('N',0x22)` | `0x40104E22` | `NcIfReq` | **W** |
| `NETCTL_SET_RADIO` | `_IOW('N',0x23)` | `0x40104E23` | `NcRadio` | **W** |
| `NETCTL_RELOAD_PROFILES` | `_IOWR('N',0x24)` | `0xC0104E24` | `NcReload` (out) | **W** |

### Queries (`NcQuery`, 32 bytes)

| Field | Direction | Meaning |
|---|---|---|
| `ifindex` | in | Filter; 0 = all |
| `elem_size` | in | The caller's element stride; 0 = the kernel's own. Must be at least the ABI-1 element size |
| `elem_size` | out | The kernel's element size |
| `cap` | in | Number of elements the buffer holds |
| `buf` | in | Pointer to the buffer |
| `count` | out | Number of elements that exist |
| `gen` | out | `scan_gen` for `GET_BSS`, the profiles generation for `GET_PROFILES`, otherwise 0 |

- The return value is the number of elements written. When `count > cap`, the first `cap` elements were written and the caller retries with a bigger buffer.
- Each element is written as `min(stride, kernel size)` bytes and zero-filled to the stride.
- The element types are the same records the events carry (below), so one decoder serves both.

### Commands

**`SCAN` (`NcScan`: `ifindex` (0 = every Wi-Fi interface), `flags`, `n_ssids` ≤ 4, `ssids[4]`)**

- A wildcard probe always runs, plus directed probes for the listed SSIDs (hidden networks).
- Returns 0 when the scan was started, joined (one is already running) or coalesced.
  - A request within `SCAN_COALESCE_MS` (10 s) of the last completed scan completes at once from the cache.
  - The flag `FORCE` bypasses coalescing. The kernel still limits fresh scans to one per 2 s.
- Every accepted `SCAN` is followed by exactly one `SCAN_DONE` per interface it covers. It is never silently dropped, which keeps NM's `LastScan` moving.
- Errors: `ENODEV` (not a Wi-Fi interface), `ENETDOWN` (radio off or firmware not ready), `EPERM`.

**`CONNECT` (`NcConnect`, 208 bytes)**

- The call is asynchronous. It returns 0 once the request is accepted and fills `attempt`, a non-zero id that is unique per boot. Progress and outcome arrive as `WIFI_STATE` events that carry that `attempt`.
- **Attempt ids strictly increase within a boot.** `CONNECT` ids are below 2^31. Attempts started by autoconnect get kernel-assigned ids with **bit 31 set** (`wifi_mgr::AUTO_ATTEMPT_BIT`), increasing among themselves. A client may therefore ignore a `WIFI_STATE` whose `attempt` is older than the one it is tracking (the NM shim does). A wired `CONNECT` returns attempt 0.
- **Profile mode** (`INLINE` clear): `profile_uuid` must name a loaded profile (`ENOENT` otherwise). Its SSID, AKM, PMF and secret are used. `BSSID` + `bssid` may pin one AP.
- **Inline mode** (`INLINE` set): `ssid`/`ssid_len` (1..32), `akm`, `pmf` and `secret`/`secret_len` (0..128) come from the struct.
  - The secret is a passphrase (8..63), 64 hex digits (a raw PSK), or later an SAE password (≤ 128).
  - `profile_uuid` is optional here, and is only a label echoed in `NcWifiState`.
  - Nothing is persisted. To save a network, write the profile first, then `RELOAD_PROFILES`, then `CONNECT` by uuid. That is the NM shim's `AddAndActivateConnection` path.
- `HIDDEN` probes the SSID directly.
- Errors and rejections:
  - An AKM this kernel cannot do (`akm::SAE` before WPA3) answers `EOPNOTSUPP` synchronously.
  - With `akm::AUTO`, a network that turns out to be SAE-only fails asynchronously with `reason::UNSUPPORTED_SECURITY`.
- Preemption:
  - A `CONNECT` preempts autoconnect and any attempt in progress. The old attempt ends `FAILED`/`SUPERSEDED`.
  - It also clears the autoconnect block left by `DISCONNECT` and the wrong-key block of that profile.
- On a **wired** ifindex `CONNECT` means "admin up": resume DHCP. Every other field is ignored.
- The kernel wipes its copy of the secret after use. Clients should wipe theirs (`NcConnect::wipe_secret`). Secrets are never logged.

**`DISCONNECT` (`NcIfReq`: `ifindex`, `flags`)**

- **Wi-Fi:** send a deauthentication and reach `DISCONNECTED` (reason `USER`) within **`DISCONNECT_DEADLINE_MS` (2 s)**.
  - nmrs calls `Device.Disconnect` before every connect and waits at most 10 s for it.
  - Any firmware restart happens after the state is reported, not before.
  - Autoconnect on that interface stays suppressed until the next `CONNECT` or `RELOAD_PROFILES` (NM semantics), unless `KEEP_AUTOCONNECT` is set.
- **Wired:** admin down. Stop DHCP, drop the address and routes at once (no carrier grace period; TCP on that interface is reset), and clear `ADMIN_UP` (oper `DOWN`). `IFACE_CHANGED` and `IPV4` (state `NONE`) follow within one net-daemon pass. A wired `CONNECT` sets `ADMIN_UP` again and restarts DHCP. Available when `features::WIRED_ADMIN` is set; without it, `DISCONNECT` on wired answers `EOPNOTSUPP`. The admin state is not persisted: every interface boots admin up.
- Returns 0 even when the interface is already disconnected.

**`SET_RADIO` (`NcRadio`: `ifindex` (0 = all Wi-Fi), `on`)**

- This is soft rfkill.
- Off means:
  - disconnect (reason `RADIO_OFF`);
  - stop scanning;
  - `BSS_GONE` for every cached BSS;
  - state `RADIO_OFF`;
  - `RADIO_ON` cleared in `NcIface.flags`.
- On returns the interface to `DISCONNECTED`, and autoconnect resumes.
- The radio state is not persisted by the kernel. The profile file's `global_flags.WIFI_RADIO_OFF` sets it at boot (see [Profiles](#profiles-file)).

**`RELOAD_PROFILES` (`NcReload`, out: `gen`, `count`, `source`, `status`)**

- The call is synchronous. It re-reads both slots, selects one, merges the legacy profile, and publishes the list to the Wi-Fi backends. It then queues `PROFILE`… `PROFILES_END` to every open.
- When it returns, `CONNECT` by uuid sees the new set.
- It clears every wrong-key block and the disconnect autoconnect block, so autoconnect re-evaluates.
- A missing or corrupt file is not an error: `status` = `NO_FILE`/`CORRUPT`, `source` = `NONE`, and the profile list holds only legacy entries. I/O errors give `status` = `IO_ERROR` and keep the previous list.

**`REQUEST_SNAPSHOT`**: queues a full snapshot (reason `REQUESTED`) to this open only. Any open may ask.

## State records

These are both `NETCTL_GET_*` elements and event payloads. Their sizes are multiples of 48 bytes. See `netctl_abi::types` for every field and offset.

| Record | Size | Parts | Content |
|---|---|---|---|
| `NcIface` | 96 | 2 | `ifindex`, `kind` (lo / ethernet / wifi), `oper` (down / lower-down / dormant / up), `name[16]`, `mac`, `perm_mac`, `mtu`, `flags` (ADMIN_UP, CARRIER, RADIO_ON, RADIO_HW_ON, READY), `speed_mbps`, `metric`, `wifi_caps` (PSK, PSK_SHA256, SAE, SAE_H2E, PMF, bands, …), `link_gen`, `driver[16]` |
| `NcIpv4` | 48 | 1 | `state` (none / requesting / bound / static), `method`, `addr`/`prefix`, `gw` (+HAS_GW), `dns[3]`/`n_dns`, DHCP `server`, `metric`, `lease_s`, `flags.DEFAULT_ROUTE` (this interface carries the selected default route: the primary connection) |
| `NcWifiState` | 96 | 2 | `state`, `reason`, negotiated `akm`, `pmf`, `ieee_status`/`ieee_reason` (raw 802.11 codes), `attempt`, `profile_uuid`, `bssid`, `freq_mhz`, `signal_dbm`, `ssid` (32), `tx_rate_kbps`, `since_ms`, `flags` (SCANNING, AUTOCONNECT_BLOCKED, AUTO, PMF_ACTIVE) |
| `NcWifiSignal` | 48 | 1 | `signal_dbm`, `quality`, `tx_rate_kbps`, `bssid` |
| `NcScanInfo` | 48 | 1 | `status` (ok / failed / aborted), `flags` (COALESCED, DIRECTED, BACKGROUND), `n_bss`, `scan_gen`, `time_ms` (u64; NM `LastScan`) |
| `NcBss` | 96 | 2 | `bssid`, `freq_mhz`, `channel`, `band`, `signal_dbm`, `quality` (NM formula), `ssid` (32), **`nm_flags`/`wpa_flags`/`rsn_flags` bit-identical to NM's `NM80211ApFlags`/`NM80211ApSecurityFlags`**, `ext` (AKM detail: PSK, PSK_SHA256, SAE, SAE_EXT, FT_*, 802.1X, OWE, MFPC, MFPR, SAE_H2E, H2E_ONLY, TRANSITION, WPA1, RSN, HIDDEN, HT/VHT/HE, **JOINABLE**), `rsn_caps`, `capability`, `beacon_int`, `age_ms`, `max_rate_kbps`, `last_seen_ms` |
| `NcProfileMeta` | 192 | 4 | `uuid`, `kind`, `akm`, `autoconnect`, `flags` (HIDDEN, HAS_SECRET, LEGACY, BLOCKED_WRONG_KEY), `priority`, `gen`, `pmf`, `ssid` (32), `id` (64, UTF-8 name), `iface[16]` (bind; empty = any), `timestamp_s`. **Never the secret** |
| `NcSnapshotInfo` | 48 | 1 | `reason` (open / resync / requested), `abi_version`, counts, `profiles_gen`, `writable` (this open may mutate) |
| `NcResync` | 48 | 1 | `dropped` (logical events lost) |
| `NcProfilesInfo` | 48 | 1 | `gen`, `count`, `source` (none / slot a / slot b), `status`, `n_legacy`, `global_flags`, `seq` |

Notes for clients:

- `JOINABLE` is set when this interface can join the BSS now: open, or PSK/PSK-SHA256 with CCMP, and SAE once supported. Clients hide or grey out other BSSs. The NM shim hides SAE-only APs and shows transition APs as PSK.
- `nm_flags`/`wpa_flags`/`rsn_flags` are computed as NM computes them (AKM 1/3/5 → 802_1X, 2/4/6 → PSK, 8/9/24/25 → SAE, 18 → OWE, 12/13 → EAP_SUITE_B_192; the CCMP/TKIP pairwise and group bits; PRIVACY from the capability field), so the shim passes them through unchanged.
- `quality` is NM's `Strength`: clamp(dBm, -100, -40), mapped linearly to 0..100 (`util::quality_from_dbm`).

## The event stream

### Records

```
offset size field
     0    1 kind      event kind (below)
     1    1 flags     bit 0 SNAPSHOT: part of a snapshot block
     2    1 part      0 .. nparts-1
     3    1 nparts    >= 1
     4    4 ifindex   interface the event is about, 0 = global
     8    4 seq       per-open logical event number (all parts share it)
    12    4 time_ms   boot-time ms when the event was generated
    16   48 payload   bytes [part*48, part*48+48) of the event's record
```

**Multi-part events.**

- A logical event is one state record. When the record is larger than 48 bytes, it is split into `nparts` consecutive records, which share `kind`, `ifindex` and `seq`. Examples: an AP report with its 32-byte SSID, or a profile with its 64-byte name.
- The payload is the concatenation of the parts. Readers zero-extend it to the struct they know.
- An event has at most `EVENT_MAX_PARTS` (8) parts in any version. ABI 1 uses at most 4.
- The kernel queues all parts of an event at once or none of them, so parts are always adjacent.
- `read(2)` returns whole records only.
  - It should not end in the middle of an event when the buffer can hold the whole event. If the buffer is smaller than the next event, it returns as many parts as fit.
  - Use buffers of at least 8 records (512 bytes).
- **Only an overflow can cut an event.** The reader then gets `RESYNC` (a new part 0) where it expected the next part. It drops the partial event and rebuilds from the snapshot that follows.
- Unknown kinds are skipped whole (`part`/`nparts` say how far).
- `netctl_abi::Reassembler` implements all of this, and `Event::encode` is the kernel-side encoder.

### Kinds

| Kind | Value | Payload | Emitted |
|---|---|---|---|
| `SNAPSHOT_BEGIN` | 0x01 | `NcSnapshotInfo` | opens a snapshot block |
| `SNAPSHOT_END` | 0x02 | `NcSnapshotInfo` | closes it; the client's model is now complete |
| `RESYNC` | 0x03 | `NcResync` | after an overflow; a snapshot (reason `RESYNC`) follows immediately |
| `IFACE_ADDED` | 0x10 | `NcIface` | an interface registered; always before any other event for its ifindex |
| `IFACE_CHANGED` | 0x11 | `NcIface` | any field of `NcIface` changed (carrier, admin, radio, oper, MAC, speed, metric, caps) |
| `IFACE_REMOVED` | 0x12 | `NcIface` | last event for that ifindex; ifindexes are never reused within a boot |
| `IPV4` | 0x20 | `NcIpv4` | any change, including the `DEFAULT_ROUTE` flag moving between interfaces (both get an event) |
| `WIFI_STATE` | 0x30 | `NcWifiState` | every `state` transition (and `reason`/`bssid`/`attempt` changes) |
| `WIFI_SIGNAL` | 0x31 | `NcWifiSignal` | while connected; at most every 2 s, only on a change of ≥ 3 dB |
| `SCAN_STARTED` | 0x32 | `NcScanInfo` | a fresh scan begins (not for coalesced requests) |
| `SCAN_DONE` | 0x33 | `NcScanInfo` | after the `BSS`/`BSS_GONE` events of that scan |
| `BSS` | 0x34 | `NcBss` | a BSS is new, or changed materially (signal ≥ 5 dB, SSID, channel, security bits) |
| `BSS_GONE` | 0x35 | `NcBss` | aged out (not seen for `BSS_AGE_OUT_MS` = 120 s) or radio off |
| `PROFILE` | 0x40 | `NcProfileMeta` | one profile of a batch (after reload, boot load, snapshot) |
| `PROFILES_END` | 0x41 | `NcProfilesInfo` | closes the batch. The client replaces its profile list with the `PROFILE` records of that `gen`. A batch may be empty |

### Snapshot on open

Every `open(2)` gets a complete snapshot before any delta, so a late opener has full state with no race against a separate query. This applies to read-only opens too. Every snapshot record carries the `SNAPSHOT` flag, and the block appears in this order:

1. `SNAPSHOT_BEGIN`, with reason `OPEN`, `abi_version`, `writable` and the counts.
2. `IFACE_ADDED` for every interface, ifindex ascending, `lo` included.
3. `IPV4` for every interface.
4. `WIFI_STATE` for every Wi-Fi interface.
5. `BSS` for every cached BSS, then the last `SCAN_DONE` of each Wi-Fi interface, if one ever completed. NM's `LastScan` is then never unknown after the first scan.
6. `PROFILE` for every loaded profile, then `PROFILES_END`.
7. `SNAPSHOT_END`.

The snapshot is generated atomically with respect to event emission. Every change after it arrives as a delta after `SNAPSHOT_END`, and no change is reported twice or lost.

### Ordering

Every open sees the same sequence of deltas, in the order the kernel made the changes.

- **Per interface:** `IFACE_ADDED` comes first and `IFACE_REMOVED` comes last.
- **A connect:** `WIFI_STATE` CONNECTING → AUTHENTICATING → ASSOCIATING → HANDSHAKE → CONNECTED. Then `IFACE_CHANGED` (carrier, oper `UP`), then `IPV4` REQUESTING → BOUND.
- **A failure:** `WIFI_STATE` FAILED(reason) is followed by DISCONNECTED (with the same reason), or by CONNECTING when autoconnect moves to another candidate. FAILED is never the last state.
- **Link loss:** `WIFI_STATE` DISCONNECTED(reason) → `IFACE_CHANGED` → `IPV4` NONE.
- **A scan:** `SCAN_STARTED`, `BSS`/`BSS_GONE`…, `SCAN_DONE`.

### Overflow and resync (lossless)

Each open has a bounded queue of `EVENT_QUEUE_RECORDS` (1024 records, 64 KiB), allocated lazily. The worst-case snapshot is 564 records, computed from:

| Item | Count |
|---|---|
| Interfaces, all Wi-Fi | 8 |
| BSS cache, kernel-wide (`MAX_BSS`) | 128 |
| Profiles (`MAX_PROFILES`) | 64 |

So a snapshot always fits.

When an event does not fit:

1. The kernel discards **everything** queued for that open and counts the dropped logical events.
2. On the reader's next `read`/`poll`, the queue holds `RESYNC{dropped}`, followed by a complete snapshot (reason `RESYNC`) generated from current state at that moment.
3. Deltas resume after its `SNAPSHOT_END`.

Every change is therefore reflected in the snapshot or follows it as a delta. The client throws its model away at `RESYNC` and rebuilds it. If the queue overflows again before the snapshot is read, the procedure restarts. A reader never sees half a snapshot followed by deltas.

`seq` counts logical events for the open, including dropped ones, so a gap is always explained by a `RESYNC`. It starts at 1 with the open's `SNAPSHOT_BEGIN`.

### read / poll

- `read(count < 64)` answers `EINVAL`. Otherwise it returns `64 × n` bytes.
- An empty queue blocks, or answers `EAGAIN` with `O_NONBLOCK`.
- `POLLIN` is reported when the queue is non-empty or a resync is pending. `POLLOUT` is never reported (commands are ioctls).
- The epoll tag is per open (`poll_class::NETCTL`, open_id), so waiters wake narrowly, as evdev does.
- `FIONREAD` (`0x541B`, `int *`) answers the number of bytes queued (a multiple of 64).
- **The queue belongs to the open file description**, not to a descriptor: `dup`/`dup2`/`fcntl(F_DUPFD)`, `fork` and SCM_RIGHTS copies share one queue (and one snapshot), and every record is delivered to exactly one `read` among them. The NM shim relies on this: a reader thread blocks in `read` on a dup while the engine issues ioctls on the original. Each `open(2)` gets its own queue and snapshot.
- Closing the last descriptor of the open frees the queue.

## Wi-Fi state machine

```
UNAVAILABLE ──fw ready──▶ DISCONNECTED ◀────────────────────────────┐
                            │  ▲                                    │
              CONNECT/auto  │  │ DISCONNECT (≤2 s) / link loss       │
                            ▼  │                                    │
   CONNECTING ▶ AUTHENTICATING ▶ ASSOCIATING ▶ HANDSHAKE ▶ CONNECTED
        │              │              │             │          │
        └──────────────┴──────────────┴─────────────┴──▶ FAILED(reason) ─▶ DISCONNECTED
any state ──SET_RADIO off / rfkill──▶ RADIO_OFF ──on──▶ DISCONNECTED
```

| `reason` | Value | Meaning | NM mapping (shim) |
|---|---|---|---|
| `NONE` | 0 | | |
| `WRONG_KEY` | 1 | 4-way MIC failure, or SAE confirm mismatch | Device 120 reason 7 (NO_SECRETS); AC state 4 reason 9 |
| `NO_SECRET` | 2 | profile has no secret | Device 120 reason 7 |
| `NO_AP` | 3 | SSID not found after a fresh scan | Device 120 reason 53 |
| `AUTH_REJECTED` | 4 | `ieee_status` holds the 802.11 status | 120 reason 8 |
| `ASSOC_REJECTED` | 5 | as above | 120 reason 8 |
| `TIMEOUT` | 6 | no auth/assoc response or EAPOL message | 120 reason 11 |
| `FW_ERROR` | 7 | firmware error/restart | 120 reason 1 |
| `UNSUPPORTED_SECURITY` | 8 | SAE-only before WPA3, TKIP, EAP, … | 120 reason 1 |
| `DEAUTH_BY_AP` | 9 | `ieee_reason` holds the 802.11 reason | 30 reason 8 |
| `BEACON_LOSS` | 10 | | 30 reason 8 |
| `USER` | 11 | `DISCONNECT` | 30 reason 39 |
| `RADIO_OFF` | 12 | `SET_RADIO` off | 20 reason 50 |
| `RFKILL` | 13 | hardware switch | 20 reason 50 |
| `SUPERSEDED` | 14 | a newer `CONNECT` replaced the attempt | — |

Rules a client may rely on:

- A profile that failed with `WRONG_KEY` is skipped by autoconnect (`BLOCKED_WRONG_KEY`) until `RELOAD_PROFILES` or an explicit `CONNECT`.
- Other failures back off before autoconnect retries them: 10 s, 30 s, then 60 s.
- One attempt is bounded by `CONNECT_DEADLINE_MS` (15 s) up to CONNECTED. DHCP comes on top of that.
- The AKM is chosen as the backend's caps allow. For `akm::AUTO` the order is SAE > PSK-SHA256 > PSK.
  - A transition AP falls back to PSK only for protocol reasons (status 77, H2E mismatch).
  - It never falls back on `WRONG_KEY`.

Until WPA3 lands, SAE is reported in `ext` and `rsn_flags` but is never in `wifi_caps`, so those BSSs lack `JOINABLE`.

## Profiles file

Saved connections live in `/etc/leandros/network/profiles.a` and `profiles.b`:

| Path | Owner and mode |
|---|---|
| The directory `/etc/leandros/network/` | `root:netdev 0770` (members write the slots and may create files here, e.g. a lock file) |
| Each slot file | `root:netdev 0660` |

mkfs pre-creates both slots, empty. **A zero-length slot counts as absent:** with every existing slot empty, `status` is `NO_FILE` (not `CORRUPT`). One empty slot plus one invalid slot is `CORRUPT`.

The model has three parts:

- **Writer:** a userland `netdev` member, normally the NM shim or the `netctl` CLI.
- **Reader:** the kernel, at boot (when the root filesystem is up, where `wifi.conf` is read today) and on `RELOAD_PROFILES`.
  - The boot load always publishes, even with no slot and no legacy network, so the first generation seen by clients and Wi-Fi backends is **1**. The legacy `wifi.conf` / `wifi.ssid=` network is merged into that same list (flag `LEGACY`). Generation 0 means "never published": until then a backend (`wifi_mgr`) autoconnects from its own legacy fallback.
- **No secrets through the node:** the kernel republishes only secret-free metadata. A non-member can see saved networks but cannot read the slot files, which hold the secrets.

The format is binary, fixed-size and little-endian. `netctl_abi::profiles` holds the parser and serializer, which are `no_std` with no allocation.

```
header (64 bytes)
     0  8  magic "LNDNETP\x01"
     8  2  version = 1
    10  2  header_size (>= 64; entries start here)
    12  2  entry_size  (>= 288; v1 writes 320)
    14  2  count       (<= 64)
    16  8  seq         (the larger valid seq wins)
    24  4  global_flags (bit 0 WIFI_RADIO_OFF: Wi-Fi soft-off at boot)
    28 36  reserved, 0
entries (count × entry_size)
     0 16  uuid (non-nil, unique in the slot)
    16  1  kind: 3 wifi (2 ethernet reserved; v1 writers store only wifi)
    17  1  akm: 0 AUTO ("wpa-psk": SAE / PSK-SHA256 / PSK per AP) | 1 NONE | 2 PSK | 3 PSK_SHA256 | 4 SAE
    18  1  autoconnect 0/1
    19  1  flags: bit 0 HIDDEN
    20  4  priority (i32, higher first)
    24  1  ssid_len (1..32)
    25  1  id_len (0..64)
    26  2  secret_len (0..128)
    28  1  pmf: 0 default | 1 disable | 2 optional | 3 required
    29  3  reserved
    32  8  timestamp_s (u64, last successful connect; 0 = never)
    40 16  iface (NUL-padded; empty = any)
    56  8  reserved
    64 32  ssid
    96 64  id (UTF-8 display name; the NM shim uses the SSID)
   160 128 secret: passphrase (8..63) | 64 hex digits | SAE password (<= 128)
   288 32  reserved (v1: zero)
trailer
     4     crc32 (CRC-32/ISO-HDLC = zlib.crc32) of every byte before it
```

**Validity.** A slot is valid when all of the following hold:

- the magic and version match;
- the geometry is sane;
- the file has at least `header_size + count × entry_size + 4` bytes (bytes after the CRC are ignored);
- the CRC matches.

The kernel loads the valid slot with the larger `seq`, and A on a tie. If neither is valid, the profile set is empty plus legacy (`status` = `NO_FILE`/`CORRUPT`). A torn or corrupt write therefore always leaves the previous slot in charge.

**Entries.** An entry is skipped, not fatal, when any of these hold. The CRC already proved the writer meant the rest.

- an unknown `kind` or `akm`;
- `ssid_len` 0 or > 32 for Wi-Fi;
- `id_len` > 64;
- an invalid secret for its AKM;
- a nil uuid;
- a uuid already seen in the slot (first wins).

Bytes of an entry beyond 288 are ignored, so entries may grow.

**Writer protocol** (`profiles::write_target`):

1. Read both slots.
2. Take `seq = max(valid seq) + 1`.
3. Serialize the full new list (`profiles::serialize`).
4. Write it to the slot that is **not** the current winner: `open(O_WRONLY|O_TRUNC)`, write, `fsync`, close.
5. Issue `NETCTL_RELOAD_PROFILES`.

Two concurrent writers serialize with `flock(LOCK_EX)` on `profiles.a` when the VFS provides it. Otherwise the last writer wins, and a writer re-reads after its reload and retries if its `seq` lost. Writers preserve entries and `global_flags` they do not understand by round-tripping the list.

**Legacy credentials.** `/etc/leandros/wifi.conf` (`ssid=`/`psk=` lines) and the `wifi.ssid=`/`wifi.psk=` cmdline keep working. This keeps SSH reachable before login on the laptop. The kernel merges them as one extra profile:

- flag `LEGACY`;
- uuid = `util::derive_uuid(UUID_DOMAIN_LEGACY, ssid)`;
- `akm::AUTO` (or `NONE` with no psk);
- priority −1000;
- autoconnect on.

It is dropped when a stored profile has the same SSID. It cannot be edited through the slots. To take it over, save a profile for the same SSID.

**Wired.** The wired interface needs no profile: it is always managed (DHCP) unless `DISCONNECT`ed. The NM shim synthesises "Wired connection 1" (uuid `derive_uuid(UUID_DOMAIN_WIRED, mac)`) and its active connection. That profile is not stored and not reported by the kernel.

## Kernel seams

- **Wi-Fi backends**: [`drivers/src/wifi_api.rs`](../../drivers/src/wifi_api.rs).
  - Backend side: `trait WifiBackend` (`driver`, `caps`, `mac`, non-blocking `submit(WifiCmd)`, `state`, `bss_snapshot`, `last_scan`) and `register(ifindex, &'static dyn WifiBackend)`.
  - Commands: `WifiCmd::{Scan, Connect(ConnectParams), Disconnect, SetRadio, ProfilesChanged}`.
  - Events: `emit(ifindex, WifiEvent::{State, Signal, ScanStarted, ScanDone, Bss, BssGone})` into the sink netctl installs with `set_event_sink`.
  - Profiles: `set_profiles`/`with_profiles`.
  - Lock order: a backend may emit while holding its own lock. netctl never calls a backend while holding its queue lock.
- **Interfaces**: the netif registry of lane L1, `drivers/src/netdev.rs`. Its names and types are listed in the `wifi_api.rs` module docs:
  - `Kind`
  - `NetifOps`
  - `register(kind, ops, idx, driver) -> ifindex`
  - `get`
  - `for_each`

## Versioning

- New ioctls take new nr values. Mutating ones are ≥ `0x20`.
- New event kinds take unused values. Old readers skip them whole.
- Structs and records grow only at their end:
  - command structs by their `size` field;
  - records by whole 48-byte parts. Readers zero-extend or truncate, `NcQuery.elem_size` handles list strides, and the profile file grows through `header_size`/`entry_size`.
- `NETCTL_ABI_VERSION` changes only for an incompatible change. A client that finds a different version must not issue commands.

## Code and tests

- [`lib/netctl-abi`](../../lib/netctl-abi): types, ioctl numbers, event encoder and `Reassembler`, profile parser and serializer, CRC-32, helpers (NM quality, channel ↔ frequency, UUID text, deterministic UUIDs, secret validation).
  - Run its host tests with `cargo test -p netctl-abi --target aarch64-apple-darwin`, or your host triple.
  - The tests pin every offset and ioctl value, round-trip every event kind including the 32-byte-SSID split, and cover torn and corrupt slots, newest-wins selection, skipped entries and newer-geometry files.
- [`drivers/src/wifi_api.rs`](../../drivers/src/wifi_api.rs): the backend seam (inert until L2/L3).

## See also

- [networking.md](networking.md): the IP stacks behind the interfaces.
- [vfs.md](vfs.md): device nodes, `dyn_dev_meta` and the open-time permission check.
- [security.md](security.md): accounts and groups.
