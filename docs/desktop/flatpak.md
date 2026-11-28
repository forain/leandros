# Flatpak, PackageKit and D-Bus activation

cosmic-store installs software from two sources, and each reaches the system a different way. Flatpak applications go through `libflatpak` linked into the store itself. Distribution packages go through a PackageKit service that answers on the session bus and runs `apk`. Both depend on busd starting services on demand. This page covers how the three fit together and why they are built this way.

## libflatpak: the Flatpak backend

`cosmic-store` is built with the upstream `flatpak` feature (`ports/cosmic-apps/build.sh` passes it with the other feature flags and patches no source). That links `libflatpak.so.0`, so the store talks to Flatpak in-process, not over D-Bus.

[`ports/libflatpak`](../../ports/libflatpak/) supplies the library. `build.sh` runs `build-in-alpine.sh` in an Alpine container and collects:

- `libflatpak.so.0` and its full `DT_NEEDED` closure (ostree, json-glib, libxmlb, libcurl, libarchive, gpgme, polkit client libraries and so on), found by walking the ELF dependencies;
- the `flatpak` and `ostree` command-line tools, the real bubblewrap (`bwrap`), `xdg-dbus-proxy` and `flatpak-session-helper`;
- the `-dev` files (headers, `.pc`), merged into the cross sysroot without clobbering, so the store's `libflatpak` crate links.

`scripts/mkfs-f2fs-populated.py` overlays the result on the image. The image's own copy of a soname wins over the closure's, and the build audits the closure for symbols the LeandrOS libc does not export. `scripts/build-all.sh` stages it through `build_libflatpak`; `LEANDROS_SKIP_LIBFLATPAK=1` skips it, in which case the store's `flatpak` feature has to be removed.

Why Alpine's build: Flatpak is a large C and GLib stack. Alpine already builds it against musl, the same libc the image uses, so rebuilding it would add nothing and the closure stays in step with the rest of the Alpine set. The CLI and bubblewrap ship with it so the package the image claims to provide is a working one, and so `flatpak install` and `flatpak run` work from a terminal as well as from the store.

Running a sandboxed application needs kernel behaviour that bubblewrap and xdg-dbus-proxy rely on (mount propagation shapes, `/proc/<pid>/root` across mount namespaces, `/proc/<pid>/ns`, `statx` mount ids, `/dev/dri` inside the sandbox). Those are kernel and VFS work and are described in [session.md](session.md) and the server pages. A network-free `flatpaktest` suite (in the `regress` suite) installs from a local OSTree repository, runs a sandboxed app and uninstalls it.

## The PackageKit facade

LeandrOS has no system bus and no PackageKit daemon. cosmic-store's `packagekit` feature still expects `org.freedesktop.PackageKit`, so [`ports/sysbus`](../../ports/sysbus/) provides a small facade, `leandros-sysbus packagekit` (`src/packagekit.rs`), that translates PackageKit transactions into `apk` operations.

- **Reads** (`Resolve`, `GetPackages`, `GetUpdates`, `SearchNames`, `GetDetails`) run `apk.static` read-only as the session user.
- **Writes** (`RefreshCache`, `InstallPackages`, `RemovePackages`, `UpdatePackages`) exec the set-user-ID-root helper `/usr/libexec/leandros-pk-helper` (`src/bin/leandros-pk-helper.rs`). The helper authorises root or a member of `wheel`, validates the package names and runs `/usr/sbin/apk`. A refusal comes back as the PackageKit error `not-authorized`; there is no polkit prompt.
- `InstallFiles` and `GetDetailsLocal` answer `not-supported`. Every transaction logs one line to stderr.
- `/usr/bin/pkcon` is a client (`leandros-sysbus pkcon search|resolve|install|remove|get-packages|get-updates|refresh|get-details`), and `leandros-sysbus pk-test` is the self-test (the `pktest` suite; it needs network).
- Store metadata comes from an Alpine appstream snapshot built by `ports/appstream-alpine/build.sh` into `/var/lib/swcatalog`.

Why a facade: the store needs a PackageKit endpoint and the system already has a package manager, `apk`, which is the authority for the image. Writing the endpoint as a thin translation keeps one source of truth for what is installed, and putting the only privileged step in a small, validating, set-user-ID helper means the service itself never runs as root. It matches how the other "system" services work here: they run as the session user on the session bus and refuse honestly what needs root (see [session.md](session.md#services-as-the-session-user-refusing-honestly)).

## busd service activation

`dbus-daemon`-style activation is what lets all of this stay idle until used. The session broker, busd, is patched (`ports/busd/start-service-activation.patch`) to read the `.service` files in `/usr/share/dbus-1/services` once at startup and to implement `StartServiceByName`, `ListActivatableNames`, `UpdateActivationEnvironment` and implicit activation when a message names an unowned but activatable name. The PackageKit service is declared in `ports/dbus/session-pkg/services/org.freedesktop.PackageKit.service` with `Exec=/usr/libexec/leandros-sysbus packagekit`. `start-cosmic-leandros` points `DBUS_SYSTEM_BUS_ADDRESS` at the session bus, so a client that asks for the "system" name `org.freedesktop.PackageKit` activates it there on its first message.

Supporting patches: `unique-activation.patch` spawns a service once however many callers ask at the same moment and uses dbus-daemon's 25 s timeout; `service-unknown-reply.patch` answers `ServiceUnknown` for an unowned name nobody can start, so absent services fail fast; `missing-destination-is-bus.patch` addresses a destination-less call to the bus, which is how xdg-dbus-proxy asks, so a Flatpak app's call to a name outside its filter gets its error at once instead of timing out. The patch table and the reasoning for activating lazily are in [session.md](session.md).

Activation is lazy because a session that never opens the store should not pay for its services, and because several applications can ask for the same service in the same millisecond during login.

## Limits

- Only the files present at busd's start are activatable; a `.service` file dropped into a running session is not seen.
- PackageKit writes need root or `wheel`; there are no repository operations and no polkit prompts.
- Flatpak installs and sandboxes work on both architectures, but anything that needs a real system bus or logind-backed policy does not.
