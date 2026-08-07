# M6 wallpaper — real default staged (2026-07-23)

## Staged path

`~/code/leandros-artifacts/m6-session-data/shared/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg`

This is the exact same absolute path this file occupies on-target:
`/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg` — the path
`cosmic_bg_config::Entry::fallback()` hardcodes (see §4 below). The old
135-byte solid-color placeholder (documented in m6-data-manifest.md §2) is
preserved alongside it as `orion_nebula_nasa_heic0601a.jpg.orig` in the same
directory (not deleted).

## Source + license

- Repo: `pop-os/cosmic-wallpapers` (submodule of `~/code/cosmic-epoch`,
  present locally but **unpopulated** — Git LFS pointers only, no `git-lfs`
  binary available on this host, `brew list git-lfs` confirms it's not
  installed).
- File: `original/orion_nebula_nasa_heic0601a.jpg` — the LFS pointer names
  this exact filename, which is what made it the obvious pick: it's the
  literal file cosmic-bg's fallback path expects, not an arbitrary
  substitute.
- Fetched the real bytes directly from GitHub's LFS media host (bypassing
  the need for the `git-lfs` CLI):
  `https://media.githubusercontent.com/media/pop-os/cosmic-wallpapers/master/original/orion_nebula_nasa_heic0601a.jpg`
  → HTTP 200, 3,839,900 bytes, JPEG 3840x2160 (matches the LFS pointer's
  declared sha256/size exactly, so integrity is good).
- License, per `cosmic-wallpapers/README.md`: source
  https://esahubble.org/news/heic0601/ ("Orion Nebula" / M42, ESA/Hubble),
  license terms at https://hubblesite.org/copyright — Hubble/ESA imagery is
  generally copyright-free for public use with attribution requested, but is
  **not** a standard CC license like some of the other wallpapers in that repo
  (e.g. the stormy-stellar-nursery and webb-inspired ones are CC BY 4.0). The
  repo's own top-level `LICENSE` file is CC BY-SA 4.0 for the repo/its other
  assets, but README.md attributes this specific image to the Hubble
  copyright page, not the repo license — attribute per hubblesite.org/copyright
  if this ships anywhere beyond an internal dev screenshot.

## Downscale

Source (3840x2160, 3,839,900 bytes) exceeded both the <3MB "pick this size"
target and is far bigger than needed for a 1280x800 QEMU framebuffer, so
downscaled with macOS `sips`:

```
sips -Z 1280 -s format jpeg -s formatOptions 85 orion_test.jpg --out orion_1280.jpg
```

`-Z 1280` fits-within-box on the longest dimension preserving the source's
native 16:9 aspect ratio (3840x2160 → 1280x720, not a forced/distorted
1280x800 — cosmic-bg's default `ScalingMode::Zoom` (config/lib.rs:196-198,
per m6-data-manifest.md) crops-to-fill on scan-out anyway, so an exact
800px height isn't required for it to fill a 1280x800 mode cleanly).

**Final staged file**: 1280x720 JPEG, quality 85, **194,826 bytes** (~190 KB,
well under the <1MB target).

## cosmic-bg fallback path-expectation finding (task step 4)

Grepped `cosmic-bg/config/src/lib.rs` (`fn fallback()`, lines 140-152):

```rust
pub fn fallback() -> Self {
    Self {
        output: String::from("all"),
        source: Source::Path(PathBuf::from(
            "/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg",
        )),
        ...
    }
}
```

The path (directory **and** filename, including the `.jpg` extension) is a
hardcoded string literal — no glob, no directory scan, no
filename-pattern-matching. It must be this exact path/name or fallback
resolution finds nothing (`None => continue`, wallpaper.rs:146, per
m6-data-manifest.md §3 — soft-fail to undrawn/black, not a crash).

Separately (already established by the earlier placeholder work, re-verified
here): `cosmic-bg` opens the file via
`ImageReader::open(path).with_guessed_format()` (wallpaper.rs:124-125), which
**content-sniffs** the format rather than trusting the extension — so the
`.jpg` in the filename is purely a naming convention for this fallback path,
not a format requirement. The `image` crate cosmic-bg uses supports
JPEG/PNG; it does **not** support JXL (per the task brief — not independently
re-verified here since we ended up needing neither PNG nor JXL, only JPEG,
which is unambiguously supported).

## Status

Both deliverables done: real image staged at the exact on-target path,
placeholder preserved as `.orig`, this note written. Not touching the
read-only repo or m6-data-manifest.md itself.
