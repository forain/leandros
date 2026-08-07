# M6 icons + eventfd nonblock — progress checkpoint

## Task 1: cosmic-icons staging

- Source: ~/code/cosmic-epoch/cosmic-icons (git submodule checkout, origin pop-os/cosmic-icons, HEAD b78b059). No fetch needed, already present locally.
- index.theme: Name=COSMIC, Inherits=Pop,hicolor, install dir per justfile is `icons/Cosmic` (capital C only).
- Confirmed the runtime-requested theme name is "Cosmic" (not "COSMIC"): libcosmic vendored at
  ~/.cargo/git/checkouts/libcosmic-41009aea1d72760b/3b85b6e/src/icon_theme.rs:9 `pub const COSMIC: &str = "Cosmic";`
  and .../src/config/mod.rs:120 `icon_theme: String::from("Cosmic")` (default). cosmic-workspaces-epoch/src/desktop_info.rs:34
  also hardcodes `.with_theme("Cosmic")`. So staging dir name share/icons/Cosmic/ is correct.
- Theme ships as plain SVGs under freedesktop/scalable/{8 categories} and extra/scalable/{8 categories}; install
  is a flat copy (freedesktop/justfile + extra/justfile: `find ./scalable -type f -exec install -Dm0644 {} $ICONS/{} \;`),
  extra/ installed after freedesktop/ and wins on 5 overlapping filenames (edit-clear, format-text-bold/italic/underline-symbolic,
  insert-text-symbolic).
- Staged full theme at ~/code/leandros-artifacts/m6-icons/share/icons/Cosmic/:
  - 672 files (671 svg + 1 index.theme), 9 dirs (+1 root = 10), 0 symlinks -> inode cost 681 (excl. root)
  - apparent content bytes: 926,883 (~905 KB); `du -sh` (host APFS, 4K block rounding on tiny SVGs): 2808 KB (~2.7-2.8MB,
    matches the earlier M6-audit "2.8MB" figure — that figure is block-rounded disk usage, not raw content bytes;
    many SVGs are far under 4KB so overhead is real and matters for f2fs too).
- Inherits=Pop,hicolor: we ship neither. Confirmed by grepping referenced icon names (see below): 4 real gaps
  (images-x-generic-symbolic, network-vpn-symbolic, preferences-desktop-display-symbolic,
  preferences-system-time-symbolic) resolve only via Pop-icon-theme inheritance we don't ship -> those specific
  icons will soft-blank even WITH cosmic-icons installed. Everything else needed at first paint is self-contained
  in cosmic-icons.
- Derived referenced-icon-name list by grepping cosmic-panel, cosmic-launcher, cosmic-settings, cosmic-notifications
  for from_name("...") plus broader "*-symbolic" string literals plus a few bare mimetype fallback literals
  (application-x-executable, folder, image-x-generic, text-x-generic): 94 unique names.
  - 81 resolve inside cosmic-icons theme.
  - 9 are illustration-appearance-*/illustration-accessibility-magnifier-applet — these are NOT theme icons at all,
    they ship embedded in cosmic-settings' own resources/icons/ (rust-embed style), confirmed present at
    cosmic-settings/resources/icons/scalable/status/*.svg — no action needed, out of scope for the icon theme.
  - 4 are genuinely missing (the Pop-inheritance gap above).
- Built pruned subset (81 referenced names -> 83 files, some names exist in >1 category dir) at
  ~/code/leandros-artifacts/m6-icons-pruned/share/icons/Cosmic/:
  - 83 files, 7 dirs (+1 root = 8), 0 symlinks -> inode cost 90 (excl. root)
  - apparent bytes 90,160 (~88 KB); du 336 KB
  - Savings vs full theme: 591 inodes (87% fewer), ~837 KB apparent / ~2.47 MB du-equivalent saved.
- Manifest with both options + mkfs snippet: ~/code/leandros-artifacts/notes/m6-icons-manifest.md

## Task 2: eventfd EFD_NONBLOCK spec
(see below, appended after kernel read)

### Key finding: the eventfd fix is ALREADY in progress, uncommitted, in the tree the other agent owns

`git status`/`git diff` (read-only) show 7 files with uncommitted changes, including
`kernel/src/syscall.rs` and `servers/vfs/src/lib.rs`, that already implement almost
exactly this fix:
- `sys_eventfd2` now forwards `flags` instead of discarding it (`_flags` -> `flags`,
  VFS_EVENTFD message now carries `initval` + `flags`).
- `servers/vfs/src/lib.rs` `handle_eventfd` now takes a `flags: u32` param, computes
  `stored = flags & (O_NONBLOCK_FL | O_CLOEXEC)`, and stores it on the new FdEntry
  instead of hardcoding `flags: 0`. Same pattern also applied to
  handle_signalfd_create/handle_timerfd_create in the same uncommitted diff.
- HEAD (committed) still has the bug exactly as described in the task (verified via
  `git show HEAD:servers/vfs/src/lib.rs` — 2-arg handle_eventfd, `flags: 0` hardcoded).

Verified the rest of the chain requires NO further code:
- `vfs::fd_nonblock(pid, fd)` (servers/vfs/src/lib.rs:652) is generic over
  FdEntry.flags for any VnodeKind — already correct.
- The generic `sys_read` fallback loop (kernel/src/syscall.rs:3692-3707) already
  checks `fd_nonblock` and returns EAGAIN immediately without yield-looping when
  set — already correct, just needed the flag to actually be stored.
- `fcntl(F_SETFL, O_NONBLOCK)` on an eventfd already works today via the fully
  generic `handle_fcntl` F_SETFL arm (servers/vfs/src/lib.rs:3662) — no
  eventfd-specific fcntl code exists or is needed.
- One genuine minor gap NOT covered by the in-progress diff: eventfd write() never
  checks for counter overflow / never returns EAGAIN on write (saturating_add,
  servers/vfs/src/lib.rs ~2908) — flagged as low-priority/optional in the spec,
  practically unreachable in this codebase's usage patterns.

Test recipe: extend userland/epolltest/src/main.rs (already has raw eventfd2 syscall
access via nr::EVENTFD2, existing test-function pattern to copy from
test_signalfd_signo/test_timeout_accuracy). Full test code drafted in the spec.

Full spec written to ~/code/leandros-artifacts/notes/eventfd-nonblock-spec.md with
exact file:line anchors for both the working-tree (in-progress) state and HEAD
(committed, still-buggy) state.

## STATUS: both deliverables complete.

## wallpaper lane (2026-07-23)

Task: replace the 135-byte solid-color wallpaper placeholder with a real
default image, staged at the exact path cosmic-bg's fallback reads.

- Located the placeholder: `~/code/leandros-artifacts/m6-session-data/shared/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg`
  (135 bytes, per m6-data-manifest.md §2 the exact hardcoded fallback path).
- Confirmed the hardcode myself: `cosmic-bg/config/src/lib.rs:140-152`
  `Entry::fallback()` → `Source::Path(PathBuf::from("/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg"))`
  — full path+filename is a literal string, no glob/scan. Format is
  content-sniffed (`ImageReader::open(path).with_guessed_format()`,
  wallpaper.rs:124-125) so extension doesn't have to match actual format,
  but we shipped a real `.jpg` anyway.
- `~/code/cosmic-epoch/cosmic-wallpapers` submodule is checked out but
  **unpopulated** — Git LFS pointers only (132-byte text files), and no
  `git-lfs` binary on this host (`brew list git-lfs` → not installed). The
  LFS pointer for `original/orion_nebula_nasa_heic0601a.jpg` conveniently
  matches the exact fallback filename already.
- Worked around missing git-lfs by curling GitHub's LFS media host directly:
  `https://media.githubusercontent.com/media/pop-os/cosmic-wallpapers/master/original/orion_nebula_nasa_heic0601a.jpg`
  → HTTP 200, 3,839,900 bytes, JPEG 3840x2160 (size matches the LFS pointer's
  declared size exactly).
- Downscaled with `sips -Z 1280 -s format jpeg -s formatOptions 85` →
  1280x720 (native 16:9 preserved, not force-distorted to 1280x800 — Zoom
  scaling mode crops-to-fill anyway), **194,826 bytes** (~190 KB).
- Staged over the placeholder; placeholder preserved as
  `orion_nebula_nasa_heic0601a.jpg.orig` in the same directory (not deleted).
- Source: pop-os/cosmic-wallpapers, image credited in that repo's README to
  https://esahubble.org/news/heic0601/, license https://hubblesite.org/copyright
  (Hubble/ESA imagery — copyright-free with attribution requested; distinct
  from the repo's own CC BY-SA 4.0 LICENSE file, which covers the repo/other
  assets, not necessarily this specific NASA/ESA-sourced image).
- Did not touch the read-only leandros repo or m6-data-manifest.md (per
  hard constraint); wrote a standalone note instead:
  `~/code/leandros-artifacts/notes/m6-wallpaper-note.md`.

STATUS: wallpaper lane complete.
