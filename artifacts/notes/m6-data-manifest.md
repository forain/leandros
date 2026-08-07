# M6 data / config-surface manifest

Host-only, repo-read-only. Companion to `m6-session-choreography.md`. Answers:
what do the COSMIC session components READ at startup, what happens when each is
missing, and what minimal data prevents a crash-or-blank-UI. All claims cited
`file:line`. Staged data lives under
`~/code/leandros-artifacts/m6-session-data/shared/` mirroring on-image paths.

---

## 1. Headline findings

- **cosmic-config never crashes on absent config.** `get()` falls through
  user→system-default→`Error::NotFound`, and every consumer wraps it
  (`unwrap_or_default()`, typed `Default`, or `Entry::fallback()`). The ONE
  precondition: `Config::new` does `create_dir_all($XDG_CONFIG_HOME/cosmic/…)`
  and returns `NoConfigDirectory` if neither HOME nor XDG_CONFIG_HOME is set
  (cosmic-config lib.rs:226-260,244). Launcher sets both → no failure path.
  Config files are **RON**, one file per key, at
  `$XDG_DATA_DIR/cosmic/<name>/v<N>/<key>` (system default) or
  `$XDG_CONFIG_HOME/cosmic/<name>/v<N>/<key>` (user) (lib.rs:234-246,465,481-487).
- **Cursor needs no data.** cosmic-comp embeds `FALLBACK_CURSOR_DATA`
  (`include_bytes!(resources/cursor.rgba)`, cursor.rs:52) and `Cursor::load`
  falls back to it when no xcursor theme resolves (cursor.rs:60-75). XCURSOR_*
  default to `"default"`/`24` (cursor.rs:295-302). **This is where anvil's cursor
  came from — a compiled-in resource, not an on-disk theme.** Confirmed
  UNNECESSARY to stage.
- **Fonts already staged by M5** (`/usr/share/fonts/{open-sans,noto}`,
  m5-session-manifest.md §3) — Open Sans + Noto Sans Mono; zero-fonts is
  soft-fail (blank text) not a crash. Not this lane's job; not re-staged.
- **Icons, .desktop, dconf/gsettings, mime**: all soft/none — see §3.
- **Wallpaper**: missing image = `None => continue` (wallpaper.rs:146), i.e. a
  **black screen, not a crash**. Staged a tiny solid-color placeholder at the
  compiled fallback path so bring-up shows a filled desktop (aids triage). See §2.

---

## 2. Staged files

```
m6-session-data/shared/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg
```

| file | on-image path | bytes | why |
|---|---|---|---|
| `orion_nebula_nasa_heic0601a.jpg` | `/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg` | 135 | 64×64 solid #27282b image. This is the **exact path `cosmic_bg_config::Entry::fallback()` hardcodes** (cosmic-bg/config/src/lib.rs:140-145). With no user/system bg config, `default_background()` returns `Entry::fallback()` → `Source::Path(this)` → cosmic-bg decodes it and (ScalingMode default = **Zoom**, lib.rs:196-198) fills the screen with a solid charcoal. Prevents a black/undrawn desktop with ~0 cost. |

Notes on the placeholder:
- It is a **PNG** despite the `.jpg` name. cosmic-bg opens with
  `ImageReader::open(path).with_guessed_format()` (wallpaper.rs:124-125), which
  **content-sniffs**, so the extension is irrelevant — it decodes fine. Verified
  decodable (`sips`: 64×64, format png).
- Arch-independent (identical on both images).
- Not real NASA imagery — a generated solid-color stand-in occupying the
  documented fallback filename. Swap in the real `cosmic-wallpapers` asset later
  for visual polish (see §5 open risks / deferred).

**Alternative (documented, NOT staged — avoids a fake filename):** instead of the
placeholder image, drop a cosmic-config system default that selects a solid color
so no image file is read at all:
`/usr/share/cosmic/com.system76.CosmicBackground/v1/all` (RON, an `Entry` with
`source: Color(Single((0.15, 0.16, 0.17)))`). Not staged because the exact RON
serialization can't be validated without a target run, and a malformed RON just
falls back to `Entry::fallback()` (the image path) anyway — so the image is the
robust primary and this is a follow-up refinement.

---

## 3. Per-component missing-file behavior

| component | reads at startup | if MISSING | crash / soft | staged? |
|---|---|---|---|---|
| **cosmic-comp** cursor | xcursor theme (XCURSOR_THEME, default "default") | embedded fallback cursor drawn (cursor.rs:52,60-75) | **soft** (pointer still shows) | no — unnecessary |
| **cosmic-comp** fonts | `/usr/share/fonts` (fontdb no-fontconfig scan) | blank text, no panic (m5-manifest §3) | **soft** | via M5 |
| **cosmic-comp** config | cosmic-config `com.system76.CosmicComp` | typed defaults | **soft** | no — defaults fine |
| **cosmic-bg** wallpaper | `Entry::fallback()` → `/usr/share/backgrounds/cosmic/orion_nebula…jpg` | `None => continue` = undrawn/black (wallpaper.rs:146) | **soft** (black) | **yes** (§2) |
| **cosmic-bg** config | `com.system76.CosmicBackground` v1 keys | `Entry::fallback()` (config/lib.rs:46) | **soft** | no |
| **cosmic-panel** config | `com.system76.CosmicPanel*` | typed `Default` panel layout | **soft** (default panel) | no |
| **cosmic-panel / applets** icons | icon theme "Cosmic" (Inherits Pop,hicolor) via `cosmic-freedesktop-icons` | lookup returns `Option::None` → blank icon widget | **soft** (blank icons) | no — deferred (cost, §5) |
| **cosmic-notifications** | wayland + notif socket fd + config | defaults; blank icons | **soft** | no |
| **cosmic-app-library** | `load_applications()` scan of `$XDG_DATA_DIRS/applications` | empty `Vec` → empty grid (app.rs:562) | **soft** (empty) | no — see below |
| **cosmic-launcher** | desktop entries + config | empty results | **soft** | no |
| **cosmic-osd** | config | defaults | **soft** | no |
| **cosmic-settings-daemon** | config + `libpipewire-0.3.so.0` (stub) | **pipewire inert** (design §2); but stub `.so` MISSING at link/load = **fatal** | see risk R1 | stub in pipewire-gap lane |

**Icon theme** — default resolved name is **"Cosmic"** (libcosmic
`icon_theme.rs:9`, `config/mod.rs:120` `icon_theme: "Cosmic"`); its `index.theme`
declares `Inherits=Pop,hicolor` (cosmic-icons/index.theme:5). Lookup via
`cosmic-freedesktop-icons` returns `Option` → a missing icon is `None`, rendered
as nothing. **No crash, no blank-screen — just iconless applets.** The full
`cosmic-icons` set is **2.8 MB across 676 SVG files (= 676 inodes)** — a large
f2fs-inode hit for a cosmetic gain, so it is **deferred** (see §5).

**.desktop / applications** — with an empty (or absent) `applications` dir,
`cosmic::desktop::load_applications` yields an empty list (app-library
app.rs:562; the `de.exec.unwrap()` at app.rs:673 is on *launch*, not load, so an
empty library can't hit it). App-library/launcher show empty and are TOLERANT
children anyway. **Confirmed unnecessary for boot.** (For a *usable* launcher
later, stage the COSMIC apps' own `.desktop` files — separate polish task.)

**dconf / gsettings** — start-cosmic exports `DCONF_PROFILE=cosmic` but **nothing
reads it**: COSMIC uses cosmic-config, not gsettings/dconf, at runtime. cosmic-
session ships `data/dconf/` + a gsettings-override only for GTK-app interop under
a real dconf, absent here. **Confirmed unnecessary** — launcher omits the var.

**mime** — `cosmic-session/data/cosmic-mimeapps.list` governs default-application
associations (xdg-open), irrelevant to session bring-up. **Confirmed
unnecessary** for M6.

---

## 4. Size / inode budget added by this lane

| item | files | inodes (incl. new dirs) | bytes |
|---|---|---|---|
| background placeholder | 1 | 1 file + up to 2 new dirs (`/usr/share/backgrounds`, `/…/cosmic`) | 135 |
| launcher `start-cosmic-leandros` → `/usr/bin` | 1 | 1 | 4583 |
| **total this lane** | **2** | **≤ 5** | **~4.7 KB** |

Negligible against the f2fs margin. (For contrast, the deferred full icon theme
would add ~676 inodes / 2.8 MB — the reason it is NOT staged now.)

---

## 5. Proposed mkfs additions (snippet — NOT applied to the repo)

```python
# --- M6: session launcher + background fallback + runtime/config dirs ---
M6D = "~/code/leandros-artifacts/m6-session-data"

# Launcher (installs where getty/login can exec it; also add to PATH).
add_file(image, host=f"{M6D}/start-cosmic-leandros",
         target="/usr/bin/start-cosmic-leandros", mode=0o755)

# Background fallback (exact path cosmic-bg Entry::fallback() hardcodes).
add_dir(image, target="/usr/share/backgrounds", mode=0o755)
add_dir(image, target="/usr/share/backgrounds/cosmic", mode=0o755)
add_file(image,
         host=f"{M6D}/shared/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg",
         target="/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg",
         mode=0o644)

# Writable dirs cosmic-config/comp/dbus need at runtime (created empty on-image;
# /run/user/0 already added by the M5 snippet — keep it).
add_dir(image, target="/root/.config", mode=0o700)
add_dir(image, target="/root/.local/share", mode=0o755)   # XDG_DATA_HOME
add_dir(image, target="/root/.cache", mode=0o755)

# settings-daemon pipewire stub (from the pipewire-gap lane) — REQUIRED or
# cosmic-settings-daemon fails to load and cosmic-session PANICS (risk R1):
PG = "~/code/leandros-artifacts/pipewire-gap"
add_file(image, host=f"{PG}/out/cosmic-settings-daemon-{arch}",
         target="/usr/bin/cosmic-settings-daemon", mode=0o755)
add_file(image, host=f"{PG}/lib/{arch}/libpipewire-0.3.so.0",
         target="/usr/lib/libpipewire-0.3.so.0", mode=0o755)

# Env is set by the launcher, NOT baked as files:
#   XDG_RUNTIME_DIR=/run/user/0  HOME=/root  COSMIC_BACKEND=kms
#   ICED_BACKEND=tiny-skia  XDG_DATA_DIRS=/usr/local/share:/usr/share
```

Deferred (optional polish, tracked for a later wave, NOT staged now):
- **cosmic-icons** "Cosmic" theme → `/usr/share/icons/Cosmic/` (2.8 MB, 676
  inodes). Only cosmetic (iconless applets otherwise). If staged, also stage
  `hicolor`/`Pop` fallbacks it `Inherits`.
- **real wallpaper** from `cosmic-wallpapers` to replace the placeholder.
- **.desktop files** for the COSMIC apps so the launcher/app-library are usable.
- **cosmic-config system-default RON** for a nicer default theme/panel/bg.

---

## 6. Top open risks for the M6 wave

**R1 — settings-daemon is fatal-at-spawn AND needs the pipewire stub `.so`.**
cosmic-session `.expect("failed to start settings daemon")` (main.rs:255) →
launch-pad `spawn().map_err` (lib.rs:198): if `cosmic-settings-daemon` is absent,
non-executable, OR its `libpipewire-0.3.so.0` stub is not on the loader path, the
exec fails and **the whole session panics**. The daemon binary is in the
*pipewire-gap* lane's `out/`, NOT `m6-session-bins/out/` — the mkfs step must pull
it and the stub `.so` from there (pipewire-gap-design.md §4). Verify `ldd`/loader
resolves `libpipewire-0.3.so.0` on-image before trusting bring-up.

**R2 — busd must grant `com.system76.CosmicSession`.** cosmic-session's
`Builder::session()?…name(…)?…build().await?` propagates any name-request failure
out of `main` via `?` (main.rs:83-90) → immediate exit. If busd's `RequestName`
is stubbed/broken, the session dies before spawning anything. busd 0.5.0 was
container-tested (m5-manifest §2) but not on-target under this exact client;
confirm no "Failed to request name" in the log.

**R3 — cosmic-comp dying before SetEnv = unrecoverable session death.** The
compositor-ready gate `env_rx.await.expect(…)` (main.rs:148) has no fallback: if
comp exits during KMS/GBM/EGL init (the M3 libgallium NULL-deref that currently
blocks kmscube is exactly this class — MEMORY handoff) before writing SetEnv, the
oneshot closes and cosmic-session panics with "failed to receive environmental
variables" — no restart. **The M3 Mesa/libgallium crash is the gating blocker
for the entire session**, not just kmscube: no comp render = no SetEnv = no
session. Everything downstream in this plan is contingent on comp getting past
that.
