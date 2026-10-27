# lane/btbluez 2026-10-27: org.bluez for the COSMIC Bluetooth applet and Settings

**DELIVERED** (D-Bus service + test); the real applet run is open (see below).

## What the clients call (cosmic-applets @ m6 src, bluer 0.17.4; cosmic-settings bluez-zbus eed01dd)
- Both: GetManagedObjects on `/`, InterfacesAdded/Removed, per-object PropertiesChanged,
  Adapter1 Address/Alias/Powered/Discoverable/Pairable/Discovering, StartDiscovery/StopDiscovery,
  Device1 Address/Adapter/Name/Alias/Icon/Paired/Trusted/Connected, Pair/Connect/Disconnect,
  AgentManager1.RegisterAgent(o,"KeyboardDisplay").
- Applet: SetDiscoveryFilter({UUIDs,Transport}) first; its discovery stream ENDS on
  Discovering=false (so the shim keeps it true across btd scan rounds); Set Trusted after pair.
- Settings: try_join over 5 adapter props (one missing hides the adapter), RequestDefaultAgent
  (failure unregisters), RemoveDevice for Forget, never picks up a LATER adapter (shim exports the
  adapter before taking the name), checks systemctl only if /run/systemd/system exists.
- Neither uses Battery1 as required (optional); both shell out to `rfkill` (failure only logged).

## What landed
- `ports/sysbus/src/bluez/` (`leandros-sysbus bluez`): see docs/servers/bluetooth.md section C.
  PropertiesChanged from a GetAll diff after every turn (NM-shim pattern).
- `ports/dbus/session-pkg/services/org.bluez.service`: activated on first use by the applet/Settings.
- btd.sock 0600 -> 0666 (the shim runs as the session user).
- bthid: `/run/bluetooth/bthid.connect` created 0666 by the daemon; requested addresses are added
  to its device list. Device1.Connect on an input-class device goes there; Connected reflects the
  btd link (held by bthid's HID channels). leandro is in group `input` (104) already.
- `bttest serve SOCK KEYS`; the scripted controller now answers L2CAP signalling and acks ACL.
- Regress: `dbus-run-session -- /usr/libexec/leandros-sysbus bluez-test` (13 cases: adapter,
  props, agent, filter, start, Discovering signal, InterfacesAdded pad (name+icon input-gaming),
  stop, Pair (+signal +key on disk), Trusted, Connect, held >5 s idle timeout, Disconnect (+signal),
  RemoveDevice (+InterfacesRemoved, key gone, not re-added)).

## Evidence
- Mac HVF: blueztest 13/13 (13.0 s), bttest 8/8. Mutation (held channel closed at once):
  connect_held FAILs as expected.

## Open
- cosmic-applet-bluetooth is not built or on the panel (only its icons are staged): it needs bluer
  -> libdbus-1 in the image; add a ports/cosmic-applet-bluetooth like the network one. Pixel-free
  applet check therefore not done. Settings' Bluetooth page (page-bluetooth, default on) uses zbus
  and should work as is; untested in a live session.
- The HID path through bthid is not exercised by blueztest (scripted controller has no HID SDP).
- No LE connect (NotSupported); no agent callbacks (passkey-entry devices fail pairing in btd).
- Shared staging: ports/sysbus/build.sh installs into ~/code/leandros-artifacts (shared by lanes).
