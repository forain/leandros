# ports/pop-launcher

[pop-os/launcher](https://github.com/pop-os/launcher) — the search backend that
`cosmic-launcher` talks to over piped stdin/stdout JSON. **Built unmodified, no
patches.** Until 2026-09-15 it was neither built nor staged: the launcher window
opened, its subscription logged `pop-launcher failed to start`, and every query
returned an empty list.

## Revision

`a332a3a` (`fix: show discrete GPU as default within cosmic context menu`,
crate version 1.2.7). This is the exact git revision `cosmic-launcher`'s
`Cargo.lock` pins for the `pop-launcher` and `pop-launcher-service` crates
(`git+https://github.com/pop-os/launcher/#a332a3a…`), and it is the
`epoch-1.3.0` submodule checked out at `../cosmic-epoch/pop-launcher`. Keep the
service and the frontend on the same revision: the wire protocol is a serde JSON
enum with no version field.

## Build

```sh
cd ~/code/leandros-artifacts/m6-session-bins
rsync -a --exclude .git ~/code/cosmic-epoch/pop-launcher/ src/pop-launcher/
rm -f src/pop-launcher/rust-toolchain        # pins "stable"; build-rust.sh uses +nightly
./gen-cargo-config.sh src/pop-launcher
./build-rust.sh src/pop-launcher aarch64 -p pop-launcher-bin
./build-rust.sh src/pop-launcher x86_64  -p pop-launcher-bin
cp src/pop-launcher/target/aarch64-unknown-linux-musl/release/pop-launcher-bin out/pop-launcher-aarch64
cp src/pop-launcher/target/x86_64-unknown-linux-musl/release/pop-launcher-bin  out/pop-launcher-x86_64
```

Both targets built first time (~1 min each on the Mac) with the same dynamic
musl-PIE recipe as every other COSMIC crate. The result is an `ET_DYN` with
`PT_INTERP /lib/ld-musl-<arch>.so.1` and `DT_NEEDED` = `libc.so` only — every
other dependency (reqwest/rustls, zbus, smithay-client-toolkit, cosmic-protocols)
is static Rust.

## Staging (`scripts/mkfs-f2fs-populated.py`)

One multicall binary serves the service and all plugins, dispatching on the
basename of `argv[0]` (`bin/src/main.rs`):

| image path                                                        | what                                  |
|-------------------------------------------------------------------|---------------------------------------|
| `/usr/bin/pop-launcher`                                           | the service (`Command::new("pop-launcher")` in `pop-launcher-service/src/client.rs`, a PATH lookup) |
| `/usr/lib/pop-launcher/plugins/desktop_entries/plugin.ron`        | plugin config, from the pinned checkout |
| `/usr/lib/pop-launcher/plugins/desktop_entries/desktop-entries`   | **hardlink** to the binary            |
| `/usr/lib/pop-launcher/plugins/cosmic_toplevel/plugin.ron`        | plugin config                         |
| `/usr/lib/pop-launcher/plugins/cosmic_toplevel/cosmic-toplevel`   | **hardlink** to the binary            |

Upstream's `justfile` symlinks each plugin name back to the binary; hardlinks
do the same job with no symlink resolution at `execve` time and no extra
content (the mkfs dedupes by host path).

The service walks `~/.local/share/pop-launcher/plugins`, `/etc/pop-launcher/plugins`
and `/usr/lib/pop-launcher/plugins` (`src/lib.rs` `PLUGIN_PATHS`), reads each
`<dir>/plugin.ron`, and execs `<dir>/<bin.path>` with piped stdio. Logs go to
`$XDG_STATE_HOME/pop-launcher/{pop-launcher,desktop-entries,cosmic-toplevel}.log`
(`/root/.local/state/pop-launcher/` in a root session; truncated at 1000 bytes).

### Plugin set

Staged: `desktop_entries` (search `.desktop` files on `XDG_DATA_DIRS/applications`;
the launcher's core function) and `cosmic_toplevel` (open-window switching over
cosmic-comp's toplevel-info protocol; `long_lived`, needs only `WAYLAND_DISPLAY`).

Not staged, because their runtime is absent from the image: `calc` (execs
`qalc`), `pulse` (PulseAudio), `pop_shell` (GNOME extension), `web`/`terminal`/
`files`/`find`/`recent`/`scripts` (browser via `xdg-open`, terminal by name, `fd`,
`recently-used.xbel`, user scripts). Add one by appending its name to the tuple in
the mkfs block — the binary already contains every plugin.

### One trap already avoided

`desktop_entries` calls `zbus::Connection::system()` and asks
`net.hadess.SwitcherooControl` for dual-GPU state **at startup, before it serves
its first search**. The system bus is aliased to the session `busd`; without
busd's `ServiceUnknown` reply (`ports/busd/service-unknown-reply.patch`) that
call would never return and the plugin would hang forever. It returns
immediately now — visible as the plugin's `starting desktop-entries` log line
landing in the same second as the first query.

## Verified (2026-09-15)

`brush /bin/start-cosmic-leandros`, then from the console
`env WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR=/run/user/0
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus … /bin/cosmic-launcher input term`
— the second instance hands the query to the session's running one over D-Bus
(`Successfully activated another instance`), the launcher opens with `term` in
the field and lists **COSMIC Terminal** (`Ctrl + 1`). Service log:
`found plugin …/cosmic_toplevel/cosmic-toplevel`, `found plugin
…/desktop_entries/desktop-entries`. aarch64/HVF and x86_64/TCG.
