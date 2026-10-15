# Lane L2 netctl — 2026-10-15

Branch `lane/netctl` (base main 8cf70b02 + lane/ncabi 29db18a3, then main df5994b6 with lane/multiif merged in).
Spec: netmgr-spec.md (decisions 1-3, lane L2); protocol: docs/servers/netctl.md (ABI 1, unchanged by this lane).

## What landed

- **`/dev/netctl`**, char 10:242, `root:netdev 0664` (`servers/net/src/netctl/`).
  - `dyn_dev_meta` arm `(10, 242)` + `GID_NETDEV = 108` in servers/vfs. The VFS mode check at open gives EACCES for O_WRONLY/O_RDWR without euid 0 or gid 108 (egid or supplementary).
  - The server records `writable` per open at `VFS_OPEN`. A mutating ioctl (nr >= 0x20) on a read-only open answers EPERM. `write(2)` answers EINVAL.
  - **Kernel change needed:** `sys_ioctl` forwarded only known ioctl types ('E', 'd', a few constants) to device servers. Every 'N' ioctl came back ENOTTY from the TTY fallback. It now routes every ioctl on the netctl device port to its server, the same way `/dev/pipewire` is routed (`net_server::netctl::device_port()`).
- **Model and events.** One cached model: links, IPv4, Wi-Fi state, BSS cache, last scan, profile metadata.
  - Snapshots and deltas are produced under one lock (`ST`, a leaf lock), so a snapshot and the deltas after it never overlap.
  - Each open has a queue of 1024 records. On overflow the queue is dropped, and the next read/poll gets RESYNC plus a snapshot (reason RESYNC).
  - Poll tag: `poll_class::NETCTL` (17) for each open. Wakes use the try-variant, deferring to the next tick, so the Wi-Fi sink is safe to call under a backend's own lock.
- **Feeds into the model:**
  - (a) the `drivers::wifi_api` event sink;
  - (b) the net server's `set_iface_hook` (carrier/address/route/DNS changes, delivered within milliseconds) together with a 250 ms `[netctl]` kthread. The kthread diffs `iface_snapshot()` and discovers wifi_api backends (driver, caps, MAC, initial state, BSS cache);
  - (c) profile loads.
- **Interface adapter** `netctl/links.rs` is the only file that touches the IP stacks:
  - It lists lo plus every registry netif (ifindex = id + 2) with IPv4 from `iface_snapshot()`. The default route comes from the net server's own choice.
  - For a Wi-Fi netif with a registered backend, oper state and flags follow the backend's state.
- **Profiles** (`netctl/profiles.rs`): `profiles.{a,b}` are read with `drivers::firmware` (the raw root read) and parsed by `netctl_abi::profiles`.
  - The newest valid slot wins. Two empty slots (as mkfs creates them) mean NO_FILE. One empty slot plus one invalid slot means CORRUPT.
  - The legacy `/etc/leandros/wifi.conf` takes precedence over `wifi.ssid=`/`wifi.psk=`. The legacy profile is merged in and dropped when a stored profile has the same SSID.
  - Loads happen at boot once the root is up (or after 60 s without one) and on RELOAD.
  - After each load the list goes to `wifi_api::set_profiles`, a PROFILE…PROFILES_END batch goes to every open, and `ProfilesChanged` goes to every backend.
  - Slot images are wiped after parsing.
  - A FAILED/WRONG_KEY state for a loaded profile sets BLOCKED_WRONG_KEY. RELOAD clears it, and so does an explicit CONNECT of that uuid. Either change re-emits the batch.
- **mkfs**:
  - `netdev:x:108:leandro`.
  - `/etc/leandros/network` `root:netdev 0770`, holding empty `profiles.a`/`profiles.b` `root:netdev 0660`.
  - `add_files_to_dir` now accepts an optional `(uid, gid)` per file; before this every packed file was root:root.
- **`/proc/<pid>/status`** has a `Groups:` line in Linux format.
- **Userland** `userland/netctl`:
  - `netctl` CLI: `status`, `ifaces`, `scan`, `bss`, `connect`, `disconnect`, `radio`, `profiles`, `save`, `forget`, `reload`, and `watch [--no-snapshot] [--for S]`. `save`/`forget` implement the A/B writer protocol.
  - `netctltest` has 43 cases. It is in runtests `regress`, and there is also a `--suite netctl`.

## Results

Release builds of both arches pass, with QEMU on the Mac (aarch64 HVF, x86_64 TCG).

| Run | aarch64 | x86_64 |
|---|---|---|
| netctltest (pre-merge, single NIC) | 43/43 | 43/43 |
| regress (pre-merge) | PASS, 22/22 | PASS, 22/22 |
| regress (after merging main/multiif) | PASS, 22/22 incl. netctltest | PASS, 22/22 incl. netctltest |
| netctltest on the final image | (in regress above) | 43/43 |

**Two-NIC check** (`LEANDROS_NIC2=1 LEANDROS_NET=user`, aarch64; script `scratchpad/nctools/nic2.py`). `netctl watch --for 50` was running while eth0 was cut and restored with `set_link net0`:

```
#10 IFACE_CHANGED eth0 lower-down          (carrier gone)
#11 IPV4 if 2 bound (default route moved)
#12 IPV4 if 3 ... (default route)          eth1 takes the default
#13 IPV4 if 2 none                         ~4 s later (L1 grace period)
#14 IFACE_CHANGED eth0 up                  link back
#15 IPV4 if 2 requesting
#16 IPV4 if 2 bound ... (default route)    5 ms later
#17 IPV4 if 3 bound                        eth1 loses the default
```

**Supplementary groups in busd-activated services: verified, no fix needed.** The run was `ffsession.py aarch64 groups --nofirefox --post "cat /proc/[0-9]*/status"`, with leandro logged in via the greeter.

- Every session process shows `Groups: 1000 29 44 104 108`. That includes the shell greetd starts, cosmic-session, cosmic-comp, busd (pid 128), and every busd child: six `leandros-sysbus` instances, `xdg-desktop-portal*` and `xdg-permission-store`.
- The chain is greetd's session worker → `initgroups` (musl, from /etc/group) → fork/exec. busd spawns through `std::process::Command` without `uid()`/`groups()`, so it never touches the group list.
- ports/greetd/README.md still says "setgroups is accept-and-ignore". That has been stale since fadbeb3b; the README was left alone because it belongs to another owner.

## Refuted premise / surprise

- "Use the existing in-kernel device pattern (VFS_IOCTL)" was not enough on its own. The ioctl reached the device only if the syscall layer recognised its type byte. `/dev/pipewire` had hit the same problem and got its own special case. A generic fix would route every DynamicDevice ioctl to its server; I did not take it, because evdev and DRM rely on the current ordering.

## ABI / doc deviations to raise with L0 (not edited here)

1. **Mode of `/etc/leandros/network`.** The lane brief says 0770 and I implemented 0770. docs/servers/netctl.md and `netctl_abi::PROFILES_DIR` say 0750. Either works for the in-place writer protocol; 0770 also lets members create files such as lock files. The doc should be updated to 0770, or mkfs changed back.
2. **Pre-created empty slots** parse as invalid (`TooShort`). The doc says "mkfs pre-creates both slots, empty" but defines no status for that case. I report NO_FILE when every existing slot is empty. Proposed doc line: "a zero-length slot counts as absent".
3. **`features::WIRED_ADMIN` is not set.** L1 has no admin-down, so DISCONNECT on a wired ifindex answers EOPNOTSUPP. CONNECT on wired is a no-op success (attempt 0). The doc's "Wired: admin down" needs an L1 hook (`set_admin(netif, up)`), and `links::set_admin` is the single place to wire it.
4. **FIONREAD (0x541B)** is answered as bytes queued. The doc does not list it; it is harmless.

## Open risks

- The Wi-Fi paths are compiled but untested at runtime: no backend registers until L3 (iwlwifi or wifi_sim) lands. That covers the sink, discovery, CONNECT/SCAN/RADIO/DISCONNECT submit, the WRONG_KEY block, and the boot WIFI_RADIO_OFF apply.
  - netctltest only checks ENODEV when `features & WIFI == 0`. It needs wifi_sim cases (scan → SCAN_DONE, connect right/wrong key, radio off → BSS_GONE).
  - Contract assumption: a backend must not call `wifi_api::emit` from inside `submit()` while it holds a lock that `state()`/`bss_snapshot()`/`caps()` also take. netctl never holds `ST` while calling a backend, so it cannot deadlock there.
- A Wi-Fi interface's IPv4 comes from the registry's per-netif stack (L1). A Wi-Fi link that only a wifi_api backend reports (an ifindex not in the registry) shows IPv4 NONE.
- The 250 ms kthread poll is now mostly a fallback. The hook covers address and route changes, and the poll still discovers backends: four wakeups per second when idle.
- A blocked read(2) on the node parks with the broadcast mask and a 10 ms deadline, because `vfs::fd_wake_tag` does not know DynamicDevice tags. poll/epoll use the narrow per-open tag.
- `/proc/<pid>/status` is built in a 512-byte buffer. With many groups (more than about 20) the tail (`Threads:`) could be truncated.
- Wired admin-down is not available (see 3).
