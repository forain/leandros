# Item 10 — busd activation cannot be boot-tested yet: neither half is in the image

Checked 2026-08-10 on the Linux box against a clean `./scripts/build-all.sh --arch
x86_64` at `6146a15`, which is four commits after `84ec91a`.

**`84ec91a` is not represented in any guest binary or config file. Booting the current
tree does not exercise it at all**, so the "44 host tests, never booted" status is
unchanged by this session — and would have been left wrongly *claimed* closed by any
boot test that only checked "does the desktop still come up".

## Both halves are staged from the artifacts tree, which predates the commit

`scripts/mkfs-f2fs-populated.py` does not build busd and does not read
`ports/dbus/session-pkg/`. It walks `~/code/leandros-artifacts/m5-session-ship/<arch>/`
and packs whatever is there.

| what | shipped from | date | verdict |
|---|---|---|---|
| `busd` | `m5-session-ship/x86_64/usr/libexec/busd` | 08-08 14:18 | md5 `984a87c6…` — **byte-identical to `busd-binaries/busd-serviceunknown-x86_64`** (08-08 10:05), the pre-activation build |
| `session.conf` | `m5-session-ship/x86_64/usr/share/dbus-1/session.conf` | 07-22 08:39 | 1 `servicedir` hit; the repo copy has 2 — the new `<servicedir>` element is absent |

So the `.service` parser and the spawn path are not in the guest, and neither is the
`<servicedir>` line that would give them anything to scan.

Do not read a `strings` hit as evidence to the contrary: the old binary already
contains `servicedir` (the commit message says busd always parsed it into
`config.servicedirs` and then never read it) and already contains
`SpawnExecFailed`/`SpawnChildExited` (standard `org.freedesktop.DBus.Error.Spawn.*`
names carried by zbus). The md5 identity is the fact that settles it.

## What *was* incidentally confirmed

The session came up cleanly and rendered a full COSMIC desktop — panel, dock,
wallpaper, ticking clock — on two independent boots. That is a genuine result for the
`service-unknown-reply.patch` regression this item worries about: **nothing hung on an
unowned name**, every libcosmic applet reached the point of drawing. But it is a result
about the *Aug-08* busd, which is the one that patch landed in. It says nothing about
`84ec91a`.

One unrelated defect surfaced and is worth its own look: `cosmic-session` panics early
in `parse_and_handle_ipc` (`core::result::unwrap_failed`, full backtrace in the session
log) while cosmic-comp and the applets carry on. The desktop survives because they are
separate processes, but cosmic-session is what relays child stderr, so **the session log
goes dead from that moment** — which is how it cost this session the cosmic-comp-side
EACCES evidence (see `item14-libseat-vt-measurement.md`).

## What a real boot test of `84ec91a` requires

1. Cross-build busd with `ports/busd/start-service-activation.patch` applied, for both
   arches. Note the shipped **aarch64** busd is older still (07-24) and predates even
   `service-unknown-reply` — that arch is a second, separate gap.
2. Stage the result into `m5-session-ship/<arch>/usr/libexec/busd`, and the updated
   `session.conf` into `m5-session-ship/<arch>/usr/share/dbus-1/`.
3. Place a `.service` file under `/usr/share/dbus-1/services` **before the session
   starts** — `Peers` scans the servicedirs once at startup, not on miss, so a file
   dropped into a running session will never be seen.
4. Testing `StartServiceByName` needs a D-Bus client in the guest; there is no
   `busctl`/`dbus-send` in the image today, and no `grep` either.

Steps 1 and 2 are the recurring shape of this landmine: `build-all.sh` builds the
input-stack shims from tracked source (`build_input_stack_shims`, added exactly to
close this kind of drift) but nothing does the same for busd. Until it does, a busd
change is committed, host-tested, and invisible to every boot.
