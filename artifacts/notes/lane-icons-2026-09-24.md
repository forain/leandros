# Lane icons — 2026-09-25

Branch `lane/icons` (base `origin/integ-wave-0924` `27f335a`), head `5c45095`, pushed.
Laptop worktree `~/Projects/leandros-icons` (x86_64/KVM). Mac worktree was scratch-only
(`~/code/leandros-icons`, branch `lane/icons-mac`, both removed after verification —
the Mac never got a persistent lane worktree since the fix needed no kernel change).

## Result: FIXED

cosmic-greeter statted ~750 icon paths per repaint (greeterlag, 2026-09-24) and found
almost none of them: `/usr/share/icons/Cosmic` was a 94-file hand-pruned subset pulled
from a host-only artifacts tree (`~/code/leandros-artifacts/m6-icons-pruned`), present
only on machines where someone had manually assembled it, and it never covered the
greeter's own buttons at all. Cross-referencing cosmic-greeter's source
(`src/{greeter,locker,common}.rs`) found the actual names it asks for:
`object-select-symbolic`, `input-keyboard-symbolic`,
`system-{suspend,reboot,shutdown}-symbolic`, `system-users-symbolic`,
`application-menu-symbolic`, `applications-accessibility-symbolic`, six
`network-*-symbolic` names, and the `cosmic-applet-battery-level-*` family — all
present upstream, none in the pruned set. A screendump before the fix showed exactly
5 of the greeter's 7 button-row icons blank, matching that list one for one.

I could not get greeterlag's `[SCPATH]` trace itself to fire in my repro (flipped
`SC_STATS` to `true`, rebuilt, confirmed the string compiled into the kernel binary,
booted to the password screen and waited 3+ minutes — zero `[SCPATH]`/`[SCSTAT]`
lines, though the greeter was visibly up and idle-repainting the clock). Time-boxed
that and used direct source cross-reference + screendumps instead, which was
sufficient to enumerate the missing names and verify the fix; did not chase why the
trace stayed silent. `SC_STATS` reverted to `false` before committing.

### Fix
`scripts/mkfs-f2fs-populated.py`: replaced the pruned-artifacts icon block with
staging from the real upstream sources, split exactly the way each project's own
justfile/Makefile installs them (checked, not assumed):
- **Cosmic** (`/usr/share/icons/Cosmic`): cosmic-icons' `freedesktop/` and `extra/`
  scalable trees (671 files, merged, extra winning the 5 same-named files) plus its
  own `index.theme`.
- **hicolor** (`/usr/share/icons/hicolor`): every per-component app/applet icon
  upstream installs there directly, NOT into Cosmic — cosmic-launcher,
  cosmic-applibrary, cosmic-workspaces-epoch (each a single flat `{APPID}.svg` ->
  `hicolor/scalable/apps`), cosmic-term (ships its own `hicolor/<size>/apps` tree),
  and all 17 `cosmic-applets/*/data/icons` trees including battery (73 files) — plus
  a real `index.theme` (Directories/Context/Size/Type sections for the sizes
  actually staged), not an empty stub.

Getting the Cosmic/hicolor split right mattered: a `cosmic-panel-button` wears its
LAUNCH TARGET's icon, not its own (`cosmic-panel-button/src/lib.rs:215-244`), so
`com.system76.Cosmic{Launcher,AppLibrary,Workspaces}` painting zero pixels looks
exactly like "the applet never started". My first pass staged only cosmic-icons and
the existing Icon= cross-check in the same script caught the regression immediately:
1 pre-existing miss (`application-default-icon`, an unrelated naming bug in
`CosmicAppletTime`'s desktop file) became 10 on that build. Fixed by adding the
hicolor per-component sources; the cross-check is back to the same 1 pre-existing miss.

Source resolution reuses `_find_cosmic_epoch()` (sessmisc's lookup order:
`$LEANDROS_COSMIC_EPOCH`, sibling of this checkout, sibling of the main checkout,
`~/code/cosmic-epoch`) instead of a hand-copied artifacts tree, and `sys.exit`s
loudly if `cosmic-icons/freedesktop`, `Cosmic/index.theme`, or any of the three
launch-target icons is missing there.

**Data synced** (laptop's `~/Projects/cosmic-epoch` was a config-only subset with no
icon sources at all): `cosmic-icons` (freedesktop+extra scalable, index.theme),
`cosmic-applets/*/data/icons` (17 components), `cosmic-launcher/data/icons`,
`cosmic-applibrary/data/icons`, `cosmic-workspaces-epoch/data/*.svg`,
`cosmic-term/res/icons/hicolor` — 720 files total, ~3 MiB, documented in
`README.subset` next to it. Not synced to the desktop (out of scope this lane; it
will hit the same loud SystemExit if its `cosmic-epoch` also lacks these).

### Image growth
744 files staged, **0.96 MiB** actual icon payload. Total image size before vs.
after (x86_64): 1782579200 -> 1799356416 bytes, **+16.00 MiB** — the extra ~15 MiB
over the real payload is f2fs's segment-granularity size quantization (one segment
class up), not icon data.

### Verification
- **x86_64/KVM** (laptop): `mkfs` output `Cosmic icon theme (cosmic-icons): 671
  file(s)`, `hicolor icon theme (per-component app/applet icons): 73 file(s)`,
  `icon themes total: 744 file(s), 0.96 MiB`; `WARNING: 1 Icon= name(s)` (the
  pre-existing one only). Greeter screendump: all 7 button-row icons render
  (accessibility, keyboard, users, settings/menu, suspend, reboot, shutdown) plus
  the password field's lock and reveal-eye icons — none of these rendered before
  except keyboard and users.
- **aarch64/HVF** (Mac): same `mkfs` counts (744 files, 0.96 MiB, same 1
  pre-existing warning) from a full `--arch aarch64` build under the Mac build
  lock. Greeter screendump: identical result, all 7 icons + password field icons
  render.
- Did not re-measure the `[SCPATH]` failed-stat-per-repaint count (see above); the
  screendump evidence (blank -> filled, both arches) is the verification this
  report relies on.

### Shared files touched
`scripts/mkfs-f2fs-populated.py` only (owned by this lane per the protocol table's
"primary files" column being empty for icons — no other lane lists it). Synced data
under `~/Projects/cosmic-epoch` and `~/code/cosmic-epoch` is gitignored/hand-synced,
not committed.

### Open
- `[SCPATH]`/`SC_STATS` didn't fire in my repro attempt — worth a follow-up if
  another lane needs that trace; I did not debug why.
- `application-default-icon` (wanted by `CosmicAppletTime.desktop`) is a pre-existing
  naming bug (no file anywhere upstream is named exactly that) — out of scope here.
- Desktop machine's `cosmic-epoch` not checked/synced for the new icon sources.
