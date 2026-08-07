# M6 cosmic-icons theme — staging manifest

## Source

`~/code/cosmic-epoch/cosmic-icons` — already checked out locally (git submodule/gitlink,
origin `pop-os/cosmic-icons`, HEAD `b78b059`). No network fetch was needed.

- `index.theme`: `Name=COSMIC`, `Inherits=Pop,hicolor`.
- Ships as two source trees of plain scalable SVGs, no build step:
  - `freedesktop/scalable/{actions,apps,categories,devices,emblems,mimetypes,places,status}`
  - `extra/scalable/{same 8 categories}`
- Install recipe (`justfile` + `freedesktop/justfile` + `extra/justfile`) is a flat copy:
  `find ./scalable -type f -exec install -Dm0644 {} $ICONS/{} \;`, `extra/` installed
  *after* `freedesktop/` and wins on 5 overlapping filenames (edit-clear,
  format-text-bold/italic/underline-symbolic, insert-text-symbolic).

## Confirmed theme directory name: `Cosmic`

Apps request the icon theme by directory name (freedesktop icon spec), not the
`Name=` field in index.theme. Grepped `cosmic-epoch` + the vendored libcosmic checkout:

- `~/.cargo/git/checkouts/libcosmic-41009aea1d72760b/3b85b6e/src/icon_theme.rs:9`
  → `pub const COSMIC: &str = "Cosmic";`
- `~/.cargo/git/checkouts/libcosmic-41009aea1d72760b/3b85b6e/src/config/mod.rs:120`
  → default `icon_theme: String::from("Cosmic")`
- `cosmic-workspaces-epoch/src/desktop_info.rs:34` → `.with_theme("Cosmic")`
- Matches the theme's own `justfile`, which installs to `icons/Cosmic` (capital C only).

So the correct install path is `usr/share/icons/Cosmic/` — this is what was staged.

## Option (a): full theme

Staged at `~/code/leandros-artifacts/m6-icons/share/icons/Cosmic/`.

| metric | value |
|---|---|
| files | 672 (671 svg + 1 index.theme) |
| directories | 9 (+1 root `Cosmic/` dir = 10 total paths) |
| symlinks | 0 |
| inode cost (files+dirs+symlinks, excl. root) | **681** |
| apparent content bytes | 926,883 (~905 KB) |
| on-disk (host APFS, `du -sh`, 4K block rounding) | 2808 KB (~2.7 MB) |

The apparent-bytes vs `du` gap (905 KB vs 2.7 MB) is real and matters for f2fs too:
most of these SVGs are far under 4 KB, so each one burns close to a full filesystem
block regardless of content size — this is exactly the earlier M6-audit "2.8MB/676
inodes" figure (that number was block-rounded disk usage, not raw content size).

## Option (b): pruned subset (referenced-at-first-paint icons only)

### Method

Grepped `cosmic-panel`, `cosmic-launcher`, `cosmic-settings`, `cosmic-notifications`
in `~/code/cosmic-epoch` for `from_name("...")` calls plus a broader sweep for any
`"*-symbolic"` string literal, plus 4 bare mimetype-fallback literals
(`application-x-executable`, `folder`, `image-x-generic`, `text-x-generic`) →
**94 unique referenced icon names**.

Resolved against the staged full theme:

- **81 names** resolve inside cosmic-icons (kept).
- **9 names** (`illustration-appearance-*`, `illustration-accessibility-magnifier-applet`)
  are **not** theme icons — they ship embedded in cosmic-settings' own
  `cosmic-settings/resources/icons/scalable/status/*.svg` (rust-embed-style app
  resources). Confirmed present there. Out of scope for the icon theme; no action needed.
- **4 names are genuinely missing** even in the full theme:
  `images-x-generic-symbolic`, `network-vpn-symbolic`,
  `preferences-desktop-display-symbolic`, `preferences-system-time-symbolic`.
  These resolve only via the `Inherits=Pop` fallback (Pop-icon-theme), which we do
  not ship in either option. **These 4 icons soft-blank regardless of (a) or (b).**
  (`hicolor` is also in the Inherits chain but is normally an empty structural
  fallback theme, not an icon source — no additional loss expected from omitting it.)

Caveat: per-application icons in cosmic-launcher/cosmic-applibrary and file-manager
mimetype icons are looked up dynamically from `.desktop` `Icon=` keys / MIME database,
not from string literals in these 4 crates' source — they are not covered by this
grep-derived list either way. The 94-name list covers panel/status-area chrome,
settings category icons, and launcher UI chrome, which is what a full-desktop
screenshot actually needs.

### Numbers

Staged at `~/code/leandros-artifacts/m6-icons-pruned/share/icons/Cosmic/`.

| metric | full (a) | pruned (b) | savings |
|---|---|---|---|
| files | 672 | 83 | 589 |
| directories (excl. root) | 9 | 7 | 2 |
| symlinks | 0 | 0 | 0 |
| **inode cost (excl. root)** | **681** | **90** | **591 (87%)** |
| apparent bytes | 926,883 | 90,160 | 836,723 (~817 KB) |
| `du` (host, 4K blocks) | 2808 KB | 336 KB | 2472 KB |

(83 files > 81 names because 2 names have matches filed under more than one category
subdir, e.g. present in both an actions/ and a status/ variant.)

## mkfs-f2fs-populated.py integration

Read `scripts/mkfs-f2fs-populated.py` (read-only, not modified). Relevant existing
mechanism (lines ~400-443): it already does a **generic recursive walk** —

```python
m4_root      = os.path.expanduser(f"~/code/leandros-artifacts/m4-input-ship/{arch}")
m4_share_src = f"{m4_root}/usr/share"
if os.path.isdir(m4_share_src):
    for dirpath, _dirnames, filenames in os.walk(m4_share_src):
        ...  # registers every dir + every regular file found, unconditionally
```

over `{m4_root}/usr/share/` for **both** `aarch64` and `x86_64`, with no extension
or path filter, and the image is dynamically sized off total required blocks (with a
2x free-space margin) — so **no script change is needed**. Dropping the icon tree
under the existing `usr/share/` staging root is sufficient:

```bash
# ready mkfs snippet — run once per arch before scripts/build-all.sh's
# mkfs-f2fs-populated.py invocation; no edits to the .py file required
for arch in aarch64 x86_64; do
  mkdir -p ~/code/leandros-artifacts/m4-input-ship/$arch/usr/share/icons
  cp -a ~/code/leandros-artifacts/m6-icons-pruned/share/icons/Cosmic \
        ~/code/leandros-artifacts/m4-input-ship/$arch/usr/share/icons/Cosmic
done
```

(Swap `m6-icons-pruned` for `m6-icons` to ship the full theme instead.)

## Inode-budget sanity check (not a hard blocker either way)

The NAT region is fixed at `SEG_CNT_NAT=2` segments × 512 blocks/seg × (4096/9 ≈ 455
entries/block) ≈ 466,000 max inode entries — nowhere near either option's cost (90 or
681). The image's total block count is computed dynamically from actual required
blocks plus a 2x free-space margin, so it self-scales rather than hitting a fixed
cap. That said, this doesn't contradict the original M6-audit caution: at 672 tiny
files the per-file bookkeeping in the python image builder (dentry packing, NAT/SIT
entries, per-file block accounting) is where the "sensitivity" likely shows up as
build-time/robustness risk, not a hard capacity wall. Given M6's exit bar is a single
desktop screenshot, **the pruned 90-inode/~88KB option (b) is the recommended
default** — it covers every statically-referenced first-paint icon at 13% of the
inode cost, and the full theme can be swapped in later at zero code risk if a
richer icon set is wanted for a later milestone.
