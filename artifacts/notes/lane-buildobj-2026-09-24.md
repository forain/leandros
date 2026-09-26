# lane/buildobj — 2026-09-24

## Task
Fix the shared `../doomgeneric` build collision (plan: `artifacts/notes/lane-pollout-2026-09-18.md`
item 2): per-worktree/per-arch OBJDIR, drop the shared `make clean`, atomic mv of
outputs.

## Root cause (confirmed)
`../doomgeneric` is a shared, non-git sibling checkout: every worktree of this
repo on the Mac resolves `../doomgeneric` to the same physical directory
(`/Users/forain/code/doomgeneric`), since they all share `~/code/` as parent.
`build_doom()` in `scripts/build-all.sh` ran `make -f Makefile.leandros ...
clean` there before every build. `Makefile.leandros`'s `clean` target was
`rm -rf x86_64 aarch64 doom-x86_64 doom-aarch64` — unconditional, both arches.
Two worktrees (or even one worktree building both arches back to back) racing
on that could delete another build's in-progress `.o` files or its finished
`doom-$arch` binary, which `scripts/mkfs-f2fs-populated.py` reads from the
fixed relative path `../doomgeneric/doom-{arch}`.

## Fix
1. `Makefile.leandros` gets `OBJDIR ?= $(ARCH)` (default preserves old bare
   behavior for a manual `make` invocation); all `.o` paths now go through
   `$(OBJDIR)/`.
2. `build-all.sh`'s `build_doom()` passes `OBJDIR="$doom_dir/.obj-$(basename
   "$ROOT_DIR")-$arch"` — unique per worktree (by the worktree directory's
   basename) and per arch — and no longer calls `make ... clean` at all.
   Within one worktree the doomgeneric source never changes mid-wave, so
   incremental rebuilds are safe and strictly faster than a full clean.
3. The final link (`doom-$(ARCH)`, still a fixed/shared path, unavoidable
   since `mkfs-f2fs-populated.py` depends on that exact name) is now atomic:
   link to `$@.tmp.$$$$` then `mv -f` onto `$@`, **both in the same recipe
   line joined by `&&`**. First attempt put them on two separate recipe
   lines — each Makefile recipe line runs in its own shell invocation with
   its own PID, so the two `$$` expansions didn't match and `mv` failed
   looking for a temp file that belonged to the other shell. Caught by an
   actual build, not just a read of the Makefile; fixed by combining the two
   commands into one shell invocation.
4. `clean` itself was narrowed to `rm -rf $(OBJDIR) $(DOOM_BIN)` (scoped to
   the current arch/OBJDIR only) so a manual invocation can't nuke another
   arch's output either. Not exercised by build-all.sh (which no longer calls
   it), but correct if anyone runs it by hand.

### Non-git sibling: vendoring decision
`doomgeneric` has no git history to carry a fix between machines/worktrees, so
a hand-edit to the sibling's `Makefile.leandros` would only ever reach the one
checkout someone touched — not the desktop machine's separate copy, and not
future fresh clones of doomgeneric. Decision: **vendor the fixed file into this
repo** at `scripts/vendor/doomgeneric/Makefile.leandros`, and have
`build_doom()` `cmp`/`cp` it over the sibling's `Makefile.leandros` whenever it
differs (idempotent — skipped once they match, so it doesn't touch mtimes on
every build). This means every machine picks up the fix automatically the
next time it runs `build-all.sh` after pulling this branch, not just the Mac
checkout edited directly during development. Rejected alternative: making
`build-all.sh` tolerate the *old* Makefile via a version probe — more moving
parts for no benefit, since vendoring the correct file is simpler and
self-healing.

## Verification

### Isolated concurrency test (`/tmp`, not the shared sibling, no build-lock needed)
Copied doomgeneric + the new Makefile.leandros to a scratch `/tmp` directory
and ran two concurrent `make` invocations against it with different `OBJDIR`s
(simulating two worktrees), 5 rounds, each round forcing a real rebuild
(`rm -f doom-aarch64; touch doomgeneric.c`) and polling the output file's size
throughout both builds' lifetime:
- All 5 rounds: both makes exit 0, final `doom-aarch64` is 1,579,552 bytes and
  a valid ELF every time, **zero transient small/torn-file observations**
  during the ~1s window both links race to produce/replace it.
- Also confirmed both arches build+link cleanly against the real
  `libleandros_libc.a` from `/Users/forain/code/leandros/userland/target/...`
  (aarch64: 1,579,552-byte ELF; x86_64: 1,653,496-byte ELF), before wiring
  OBJDIR into build-all.sh.

### Full build-all.sh + boot, from the real worktree
`git worktree add ~/code/leandros-buildobj -b lane/buildobj origin/main`.
Waited for the shared build lock, then ran `SDKROOT=.../MacOSX26.5.sdk
./scripts/build-all.sh --arch both` under the lock from
`~/code/leandros-buildobj` (full build, all sibling ports, both arches).
(Note: this Mac's default `xcrun`-resolved SDK, MacOSX27.0.sdk, is broken —
"tapi error: malformed file" on host-side build-script linking — pinning
SDKROOT to MacOSX26.5.sdk is required on this host for ANY build-all.sh run,
unrelated to this fix; lane/execleak hit and worked around the same thing
independently.)

**Result: BUILD SUCCEEDED, both arches, exit 0.** Confirmed live in the real
shared `../doomgeneric` (not `/tmp`): build log shows "Updating
.../doomgeneric/Makefile.leandros from vendored copy..." then both `doom-aarch64`
and `doom-x86_64` compiled into their own `.obj-leandros-buildobj-{arch}/`
dirs and linked atomically (`mv -f doom-$ARCH.tmp.$$ doom-$ARCH`), no `make
clean` call. Both got packed into their F2FS images: `Packed doom (size:
1579824 bytes...)` (aarch64) and `Packed doom (size: 1653616 bytes...)`
(x86_64), plus `doom1.wad` (4196020 bytes) both times — sizes match the
just-linked binaries exactly (verified via `file`/`ls` on
`../doomgeneric/doom-{arch}` too). Lock released automatically the moment
`build-all.sh` exited (wrapper's EXIT trap), before any boot step — another
lane (vfsmisc) picked it up immediately after.

Booted both via `.claude/skills/run-leandros/driver.py` from the worktree:
- **aarch64 (HVF)**: boots to login, `root`/`root` login succeeds, `/bin/doom`
  (1,579,824 B) and `/bin/doom1.wad` present; ran `doom` — full init sequence
  (zone alloc, WAD load, MIDI/soundfont, DRM 320x200 framebuffer), reaches the
  game loop cleanly, no crash.
- **x86_64 (TCG)**: same — boots to login, logs in, `/bin/doom` (1,653,616 B)
  present, runs the same full init sequence to the game loop cleanly. A few
  `[PW] producer gap` lines during startup are the documented, benign
  self-healing audio behavior (see run-leandros skill notes), not a fault.

Both QEMU instances stopped cleanly afterward.

## Files touched
- `scripts/build-all.sh` (`build_doom()`)
- `scripts/vendor/doomgeneric/Makefile.leandros` (new, vendored)
- `/Users/forain/code/doomgeneric/Makefile.leandros` (shared sibling, updated
  by the vendoring copy the first time build-all.sh ran under this branch —
  not a repo file, not committed, affects all worktrees on this Mac
  immediately)

## Branch
`lane/buildobj`, pushed to `origin/lane/buildobj`.
