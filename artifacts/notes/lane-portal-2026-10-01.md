# Lane portal (2026-10-02): FIXED

The lane delivered org.freedesktop.portal.Desktop with the COSMIC backend. FileChooser, Screenshot and Settings work on both arches (Access too, since Screenshot needs it). No COSMIC source was patched.

Branch `lane/portal` @ `6593739` (base origin/main a30f369), pushed.

## Design: option (a), at much lower cost than expected
- **Frontend.** The frontend is Alpine 3.21's prebuilt `xdg-desktop-portal 1.18.4` plus `xdg-permission-store`, built in a container and ELF-fixed like ports/firefox (libc soname, ssp shim). Its GLib closure is about 9 MB. `xdg-document-portal` is not shipped: it needs FUSE, and host apps get plain file:// URIs without it.
- **Backend.** The backend is `xdg-desktop-portal-cosmic`, cross-built unmodified from the pinned cosmic-epoch submodule (epoch-1.3.0, 151120e) with the m6 toolchain.
  - GLib/GIO come from Alpine's glib-dev, wired in through system-deps overrides. The portal's Cargo.toml hardcodes cosmic-files' `gvfs` feature, which is why GLib is needed.
  - The libpipewire stub is now a superset (78 pw_* functions, still 0/NULL).
- **ScreenCast is not advertised.** Our `cosmic.portal` drops ScreenCast, because PipeWire is only a stub here.
- **Wiring.** `ports/portal/build.sh` assembles `out/<arch>/`. mkfs overlays that tree using a helper now shared with Firefox, and prefers the superset pipewire stub. build-all has a `build_portal` step (stamp-skipped).

## Bugs found and fixed on the way (all measured)
1. **vfs had no `/proc/<pid>/root`.** xdg-desktop-portal opens it for every call to look for `.flatpak-info`, so every portal call was refused with AccessDenied "Unable to open /proc/N/root". It now resolves to `/` for a live pid, in both open and stat.
   - Second half of the bug: mount servers re-read the path pointer, so the rewritten path has to be forwarded explicitly.
2. **busd spawned an activated service once per concurrent caller.** cosmic-app-library and cosmic-launcher hit the portal in the same millisecond, which gave 2 frontends and 2 backends of 60 MB each.
   - `unique-activation.patch` now waits on a spawn already in flight.
   - The activation timeout went from 5 s to dbus-daemon's 25 s. The frontend activates its backend before claiming its name and took more than 5 s during the login storm, so apps got ServiceUnknown.
3. **RealtimeKit1 was missing** (3 warnings per frontend start). It is now `leandros-sysbus rtkit`: zero limits, NotSupported for every request.

## Evidence
| | aarch64 HVF virgl (Mac, gles31) | x86_64 KVM virgl (desktop) |
|---|---|---|
| portal ServiceUnknown in session log | 0 (baseline 6/login) | 0 |
| "Failed to get the color scheme/contrast" | 0 | 0 |
| frontend instances / lost-name backends | 1 / 0 | 1 / 0 |
| `leandros-sysbus portal settings` | 4/4: color-scheme 1 (dark), accent (0.388,0.816,0.875), contrast 0 | 4/4 |
| Screenshot (non-interactive) | access dialog then Allow, PNG 1280x800 (1.25 MB); repeat call 2.5 s, no dialog | PNG 1920x1080 (2.06 MB); repeat call 247 ms |
| FileChooser from probe | dialog, picked file returned `file:///usr/share/backgrounds/cosmic/orion…jpg`, exists=true | n/a |
| cosmic-settings, Wallpaper, "Add image" | "Choose wallpaper image" dialog within 9 s, nebula.jpg added (2nd thumbnail) | dialog within 4 s, added |
| exc/PF/SEGV/panic/WDOG during session | 0/0/0/0/0 | 0/0/0/0/0 |
| polltest / drmsmoke / vfstest (--no-gpu) | 18/18, failed=0, 59 pass | 18/18, failed=0, 59 pass |
| leandros-sysbus probe | 13/13 | n/a |

Screenshots are in the scratchpad `pt/a64e`, `pt/a64d` and `pt/x64`.

## Open
- **Slow first dialog on the Mac.** The first file-chooser dialog took about 60 s to show on Mac HVF/virgl (window created → first frame). Later dialogs took under 9 s, and x86 KVM took under 4 s. This is most likely wgpu/EGL init on ANGLE (cosmic-workspaces shows ~19 s for the same step). Not investigated further.
- **One warning per login remains.** `busd: unknown destination org.freedesktop.portal.Documents` comes from the frontend at startup. Fixing it needs a document portal, which needs FUSE.
- **No ScreenCast.** It needs a real PipeWire.
- **The x86_64 backend can't be built on the desktop.** The desktop's cosmic-epoch is a data-only subset. The x86_64 backend was built on the Mac and rsynced into `ports/portal/out/x86_64`. Build outputs are preserved in `~/code/leandros-artifacts/portal-out/`.
  - To use them, copy that tree to `ports/portal/out/` and `touch out/<arch>/.stamp`, or set `LEANDROS_PORTAL_BACKEND=<binary>`.
- **The screenshot permission is per-user state.** It is stored by xdg-permission-store (~/.local/share/flatpak/db), so the first non-interactive screenshot by a host app asks "Allow Applications to Take Screenshots?". This is upstream behaviour.
- **Shared files touched:** `scripts/mkfs-f2fs-populated.py` (Firefox overlay refactored into `_overlay_port_tree`; pipewire stub preference), `scripts/build-all.sh`, `servers/vfs/src/lib.rs`, `ports/busd`, `ports/sysbus`.
