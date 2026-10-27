# lane/cosmicbt 2026-10-27: Bluetooth applet build, bluetooth group, sysbus staleness guard

**PARTIAL**: items 1, 3, 4 DELIVERED; item 2 (live COSMIC session check) open, see handoff.

## Delivered
- `ports/libdbus/build.sh`: libdbus-1 1.16.2 (client library only, meson, sha256-pinned) into the m3
  sysroot + `~/code/leandros-artifacts/libdbus/out/<arch>/libdbus-1.so.3`; mkfs ships /usr/lib/libdbus-1.so.3.
- `ports/cosmic-applet-bluetooth/` (build.sh + upstream .desktop): unmodified cosmic-applets, bluer 0.17.4.
  Both arches build (aarch64 40 s); DT_NEEDED = libdbus-1.so.3, libxkbcommon.so.0, libc.so. Staged by mkfs
  together with libdbus + .desktop or not at all; build-all.sh calls it. Upstream's default plugins_wings
  already names com.system76.CosmicAppletBluetooth, so staging the .desktop puts it on the panel.
- Permissions: group `bluetooth:x:112:leandro`; btd.sock and /run/bluetooth/bthid.connect are now
  root:bluetooth 0660 (were 0666). bttest: `sock_group_member_allowed` (uid 1000 + gid 112 gets INFO) and
  `sock_outsider_refused` (uid 1001 cannot connect) PASS.
- Shared-artifact guard: leandros-sysbus embeds `leandros-sysbus-services:<all subcommands>;` (SERVICES in
  ports/sysbus/src/main.rs, incl. bluez and power). mkfs refuses the image unless the staged binary's list
  equals this tree's and names every .service's subcommand. Shown failing: the btbluez-era binary with a
  NEWER mtime (touched) -> "STALE ... staged serves [no marker]" for every sysbus .service.

## Evidence (Mac HVF aarch64)
- bttest 0 failures (incl. the 2 new cases); blueztest 13/13 RC=0 (13.1 s).
- x86_64 full regress on the box: see merge commit message / report.

## Open
- Item 2: live session (applet <-> org.bluez against `bttest serve`, Settings Bluetooth page) not run;
  handoff in the orchestrator scratchpad handoff/cosmicbt.md.
- Not shown failing on unfixed code: sock_outsider_refused (would need a 0666 rebuild).

## Continuation (cosmicbt3, 2026-10-27)

### BUG 1 - FIXED: busd delivered signals to rules whose well-known sender had no owner
`ports/busd/wellknown-sender-unowned.patch`: `MatchRules::matches` skipped the sender
check while `org.bluez` was unowned, so bluer's InterfacesAdded rule received other
services' signals during activation and panicked (`Object path was not valid: ''`,
bluer session.rs:479), killing the applet's bluer task. 1 panic before, 0 after.
busd rebuilt and staged for both arches.

### BUG 2 - FIXED: btd could never listen on /run/bluetooth/btd.sock (errno 95)
AF_UNIX nodes bind only on tmpfs, and /run (apart from /run/user) was on the F2FS root.
The fix follows Linux: `/run` is now a VFS tmpfs root, with `/run/user` nested inside it
(`TMPFS_ROOTS` = /tmp, /dev/shm, /run/user, /run; the intercept takes `/run/...`).
Audit of /run before the change: everything there is runtime state that its owner
recreates at boot: `/run/greetd-init.pid` and `/run/greetd.run` (init),
`/run/cosmic-greeter` (init mkdir+chown), `/run/espctl.lock` (espctl, O_CREAT),
`/run/bluetooth` (btd mkdir). mkfs stages only /run, /run/user and /run/user/0, and the
VFS already seeds /run/user/0. `/run/pipewire/pipewire-0` (`force_bind_unix`) was never
reachable by path connect (connect resolves via the VFS) and is unchanged.
Live: `/run/bluetooth/btd.sock` is `srw-rw---- root:bluetooth`, the shim logs
`adapter 11:22:33:44:55:66 "bttest" ready`.
Not done: socket binds on f2fs. It would need an S_IFSOCK inode type in the f2fs server
and a VFS map from (dev, ino) to the live socket id. Nothing needs it now.

### Live session (btsession.py aarch64, HVF, virgl)
- The applet shows the adapter powered. Expanding "Other Bluetooth devices" lists
  "LeandrOS Test Pad". Clicking it gives shim `pair 06:05:04:03:02:01`, and /tmp/bt.keys
  gains the address plus its key. Clicking again gives `connect 06:05:04:03:02:01 (HID)`
  (no bthid in QEMU, so it falls back to a generic connect).
- "Bluetooth settings..." opens cosmic-settings on the Bluetooth page: adapter "bttest"
  visible, Test Pad "Connected".
- `--no-bluetooth`: leandro is not in the bluetooth group (`id`), and the session never
  logs `adapter ... ready` after 30+ s (the member run logs it within seconds).
  Note: the shim only logs `btd unreachable` on a ready-to-gone transition, so an
  outsider is silent rather than noisy.
