# leandros-sysbus

Minimal `org.freedesktop.login1`, `org.freedesktop.locale1` and
`org.freedesktop.UPower` services for the COSMIC session. One static-musl
binary; `argv[1]` picks the service. busd starts each one on first use via the
`.service` files in `ports/dbus/session-pkg/services/`.

LeandrOS has no system bus. `start-cosmic-leandros` points
`DBUS_SYSTEM_BUS_ADDRESS` at the session busd, so these "system" services run
there, as the session user. Anything that needs root fails with an error and
never pretends to succeed:

- login1: `PowerOff`, `Reboot` and `Halt` are forwarded to init over
  `/run/user/initctl`; init authorises the request from the socket's peer
  credentials (root, or a process in a local session, as logind's default
  policy) and does the orderly shutdown and reboot(2). `CanPowerOff`,
  `CanReboot` and `CanHalt` say `"yes"` while init listens. `Suspend`,
  `Hibernate` and the other sleep states return `NotSupported`, and their
  `Can*` return `"na"`.
- locale1: `SetLocale` fails with `AccessDenied` when `/etc/locale.conf` is
  not writable. `SetX11Keyboard` takes effect on the bus and is persisted
  best-effort, because it only mirrors cosmic-comp's own xkb config.

What does work: sessions, seat and user objects; the idle and locked hints;
inhibitors (`Inhibit` returns a pipe fd, and the inhibitor is released when
the client closes it); and a UPower that reports a machine with no battery,
as upower does on a desktop.

`leandros-sysbus probe` is a client. It exercises all three services the way
COSMIC components do (it never calls PowerOff/Reboot) and prints PASS/FAIL lines, which makes it the in-guest
check:

    DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/user/1000/bus /usr/libexec/leandros-sysbus probe

Build: `./build.sh [aarch64|x86_64|both]`. `scripts/build-all.sh` calls it
from `stage_dbus_session`, and mkfs refuses to build an image whose staged
binary is missing or older than these sources.
