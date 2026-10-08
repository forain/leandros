# Lane dbussvc (2026-10-02): PARTIAL (4 of the 5 missing names and the Desktop dir are FIXED; the portal is left open)

Branch `lane/dbussvc` @ `c211ac2` (base origin/main 6703408), pushed. No COSMIC sources were patched and no kernel change was made.

## Phase 1: who calls what, and impact (measured: baseline session log plus screenshots, aarch64 HVF virgl)
| name | callers (shipped) | user-visible effect when missing | effort | done |
|---|---|---|---|---|
| org.freedesktop.login1 | cosmic-osd (session id for the polkit agent; PowerOff/Reboot on the shutdown dialog), settings-daemon (PrepareForSleep, SetBrightness), cosmic-comp/session (inhibitors) | the polkit agent never registers; Confirm on the shutdown dialog fails | small | yes |
| org.freedesktop.locale1 | cosmic-settings Region & language; settings-daemon xkb mirror | Region page empty (no language, blank Formatting). It **also** needs iso-codes JSON: locales-rs fails first without it | small, plus a data package | yes, plus iso-codes |
| org.freedesktop.UPower (+ .PowerProfiles) | cosmic-idle (OnBattery), osd/settings-daemon (KbdBacklight), Settings Power page | Power page shows "Backend not found. Install system76-power or power-profiles-daemon" | small | yes (no battery; ppd placeholder driver) |
| org.freedesktop.PolicyKit1 | cosmic-osd agent registration (reached only once login1 exists) | warning on every login; no auth dialogs | small | yes (agent registry; only root is authorized) |
| org.freedesktop.portal.Desktop | libcosmic theme fallback (color-scheme/contrast, 6 errors per login), Settings wallpaper/theme file chooser, cosmic-screenshot (not shipped) | file choosers fail; error noise | large: C xdg-desktop-portal + glib, or a Rust Settings-only shim that must track cosmic-config to stay correct | **no** |
| "Failed to activate another instance" | libcosmic single-instance probe (`app/mod.rs:249`) | none. The first instance logs this WARN by design on any distro, and the second instance works (power key → `cosmic-osd shutdown` reaches the running OSD; the dialog appears in the baseline) | n/a | REFUTED as a bug |

## Implemented
- `ports/sysbus`: one static-musl zbus binary with subcommands `login1`, `locale1`, `upower`, `power-profiles` (claims both org.freedesktop.UPower.PowerProfiles and net.hadess.PowerProfiles), `polkit`, and the client `probe`. The services exit when busd goes away (verified on host). Root-only operations (PowerOff, writing /etc/locale.conf as uid 1000) return NotSupported or AccessDenied and never claim success. Inhibitors are real (pipe fd, released on close).
- `ports/dbus/session-pkg/services/*.service` (6 new). Activation goes through the session busd, which DBUS_SYSTEM_BUS_ADDRESS already aliases.
- `ports/iso-codes/build.sh`: Debian iso-codes 4.18.0 (sha256-pinned), the 3 tables locales-rs reads.
- `start-cosmic-leandros`: the xdg-user-dirs-update equivalent on first login (8 dirs + user-dirs.dirs; an existing file is left alone).
- `build-all.sh stage_dbus_session` builds both; mkfs packs leandros-sysbus 0755 and refuses an image when a .service Exec= binary is missing or older than its sources.

## Evidence (scratchpad ds/: base-a64, lane2-a64; desktop ds-x64b)
| | aarch64 HVF virgl (Mac) | x86_64 KVM virgl (desktop) |
|---|---|---|
| ServiceUnknown for login1/locale1/UPower/PolicyKit1 | base 2/1/1/0 (+5 UPower unknown-dest) → **0/0/0/0** | **0/0/0/0** |
| Desktop-dir warnings | 6 → **0** | **0** |
| `leandros-sysbus probe` | **13/13** | **13/13** |
| Region page | empty → English (US), formatting filled | filled |
| Power page | "Backend not found" → 3 power modes, Balanced selected | same |
| Power key → Confirm | osd → login1.PowerOff → NotSupported logged, desktop stays up | same |
| exc/PF/SEGV/panic/WDOG | 0/0/0/0/0 | 0/0/0/0/0 |
| polltest / drmsmoke (--no-gpu) | 18/18, failed=0 | 17/18 (`poll_linux_semantics`), failed=0 |
The single x86 polltest FAIL is environmental. The desktop's ../relibc lacks `fd1967e1 poll: Linux semantics` (lane relibcpoll), which the Mac checkout has. This lane changes no relibc or kernel code.

## Open
- portal.Desktop (6 errors per login; no file chooser or screenshot). The next step would be a Rust org.freedesktop.portal.Settings shim that watches cosmic-config, or a build of xdg-desktop-portal + xdg-desktop-portal-cosmic (needs glib/pipewire).
- PowerOff/Reboot are impossible: there is no reboot(2) syscall, and services run as the session user (no system bus, no setuid). Fixing that needs a kernel reboot path (PSCI SYSTEM_OFF / ACPI S5) plus a privileged broker.
- Also still missing: timedate1 (Date & time page), net.hadess.SwitcherooControl, org.a11y.Bus. `locale -a` is absent, so "Add language" has an empty list.
- SetLocale as uid 1000 → AccessDenied (/etc is root's).
