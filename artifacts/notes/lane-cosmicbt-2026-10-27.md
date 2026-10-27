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
