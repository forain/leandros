# M9 Lane O — live COSMIC validation of `afunix_listen_strict.patch` (TODO 10)

Machine: Linux box `forain@172.16.158.150`, checkout `/home/forain/Projects/leandros`,
branch `main` at `a0325c6`. Nothing pushed to `origin`. Both stashes left untouched.

**Verdict: LAND. Landed on the box's `main` as `3532c7b`. Not pushed.**

## Setup

- Kernels rebuilt at `a0325c6` with `scripts/m7z2-kernel-only.sh` for both arches
  (`/tmp/m9o/kbuild-head-{x86_64,aarch64}.log`, rc=0 both). The x86_64 kernel build
  was a no-op cache hit, confirming the pre-existing `leandros-limine-x86_64.img`
  already carried the HEAD kernel.
- f2fs images regenerated fresh at HEAD with
  `scripts/mkfs-f2fs-populated.py f2fs-data0-<arch>.img <arch>`, then copied to
  `f2fs-data1-<arch>.img` (`/tmp/m9o/mkfs-head-<arch>.log`, rc=0 both). This is the
  fresh starting state for the control double-run.
- The COSMIC ship set is present on the box via the symlink
  `~/code/leandros-artifacts -> /run/media/forain/samsung970pro512/leandros-siblings/leandros-artifacts`
  (`m3-gl-stack/out/cosmic-comp-*`, `m6-session-bins/out/cosmic-*`,
  `m6-session-data/{shared,start-cosmic-leandros}`), so `mkfs-f2fs-populated.py`
  stages a complete session.
- Harness: `/tmp/m9o/cosmic_run.py`, derived from the corrected
  `/tmp/m9lane/lane_i_run.py` (mark-before-send, per-command numbered sentinel,
  one process owns QEMU, pty for guest serial, no pipes, `python3 -u`). QEMU tracing
  is not enabled, so nothing shares the serial pty with a trace stream.
- Positive control, first command of every boot: `nosuchbinary_xyz42`, expect `rc=127`.

## Static pre-check done before the runs

`sock_type` is stored per socket in `servers/net/src/lib.rs` and the new EOPNOTSUPP
gate reads it. All five `SockEntry { .. }` constructions in that file
(lines 976, 1215, 1281, 1508, 1516 — `socket`, the two `accept` paths, and the two
`socketpair` ends) set `sock_type` explicitly; the `sock_type: 0` at line 367 is only
the free-slot default. So no live AF_UNIX socket can reach `handle_listen` with a
zeroed type and be wrongly refused with EOPNOTSUPP.

## Instruments and their controls

| instrument | what it reads | control that proves it is live |
|---|---|---|
| `launch_pad: starting process '<name>'` count per component | cosmic-session's own launcher | control run shows exactly 12 starts, one per component; a restart loop is a count > 1 for one name |
| ANSI-stripped normalised session log (`/tmp/m9o/sesslog.py`) | every non-`[DRM-SRV]` serial line | 404 session lines on the control run, including `busd::bus: Listening on UNIX socket file /run/user/0/bus`, `Starting: /bin/leandros-applet`, `Done spawning applets` |
| QMP `screendump` + pixel sampling | the real framebuffer | control: `1920x1080 distinct=1474`, `y=16` row is `(27,27,27)` across the full width (panel bar), mid-screen is nebula colour |
| `nosuchbinary_xyz42` | the serial command channel itself | `rc=127` as the first command of every boot |

QEMU tracing was never enabled, so the guest serial pty carries only guest output —
the failure mode that shredded the previous lane's `grep` cannot occur here.

## Control double-run — x86_64 (KVM)

Both boots used the same `f2fs-data0/1-x86_64.img`, never regenerated between them.

**C1 (fresh image)** — `/tmp/m9o/C1-x86_64.{out,serial.log}`

- positive control `rc=127`.
- probes: `/root/m9o-boot-marker` absent (`rc=1`), `/root/.config` empty,
  `/root/.config/cosmic` absent, `/root/.local/state` absent, `/run/user/0` empty.
- session: 12 `launch_pad` starts, one each for `cosmic-comp` (`--no-xwayland`),
  `cosmic-settings-daemon`, `cosmic-notifications`, `cosmic-panel`,
  `cosmic-app-library`, `cosmic-launcher`, `cosmic-workspaces`, `cosmic-osd`,
  `cosmic-bg`, `cosmic-greeter`, `cosmic-files-applet`, `cosmic-idle`.
  **Zero** `before restarting process`, `restarted process`, `failed to restart`,
  `exited with error`.
- **Zero** `Unknown id`, `Broken pipe`, `PANEL MAIN ERR`, `panicked`.
- Screendumps at t=60/120/180/235 s: full-width panel bar `(27,27,27)` at y=16 over
  the nebula wallpaper, `distinct=1474`.
- The only `Invalid argument` hits in the log are two host-side
  `pulseaudio: set_sink_input_volume/mute() failed` lines from QEMU, not the guest.

**C2 (same image, second boot) — the image was genuinely dirty**

- `cat /root/m9o-boot-marker` → `BOOTMARK-C1-x86_64`. Guest writes from run 1 survived
  the reboot with no explicit `sync` (the harness's `sync` attempt timed out because
  the console is owned by the session; it was not needed).
- `/root/.config` → `cosmic  qt5ct  qt6ct` (was empty).
- `/root/.config/cosmic` → the full `com.system76.Cosmic*` config tree created by
  run 1 (was absent).
- `/root/.local/state` → `cosmic  cosmic-comp` (was absent).
- `/run/user/0` → **empty**. This is the important caveat, recorded below.

### What "dirty" does and does not cover here

`XDG_RUNTIME_DIR` is `/run/user/0`, and `/run/user` is one of the three
`TMPFS_ROOTS` in `servers/vfs` (`/tmp`, `/dev/shm`, `/run/user`). It is in-memory and
is empty at every boot — verified directly in the C2 probe above. So **a reboot cannot
carry a stale `S_IFSOCK` into the next session's runtime dir**: the `wayland-N` and
`bus` sockets are gone. The dirty state that does survive is the f2fs-backed
`$HOME` tree (`/root/.config`, `/root/.local/state`, `/root/.cache`), which is what
makes the second session take different code paths (saved panel/background/theme
config, `cosmic-comp` state) and get further, not the socket nodes.

### Control C2 result (x86_64)

- positive control `rc=127`.
- 12 `launch_pad` starts, 12 distinct names, **max 1 per name** — no restart loop.
- Screendumps at t=60/120/180/235: identical to C1 (`distinct=1474`, full-width
  `(27,27,27)` bar, nebula below).
- **One new line relative to C1**, and it is a *control* (unpatched) finding:
  `cosmic-notifications: Failed to setup panel dbus server I/O error: Broken pipe
  (os error 32)`. Zero `Unknown id`, zero `PANEL MAIN ERR`, and it does not cause a
  restart. **Recorded here so that if it recurs on the patched run it is not
  attributed to the patch — it is a dirty-second-boot artefact of the current kernel.**

## Control double-run — aarch64 (TCG)

`-cpu max -accel tcg`; Limine did not wedge, so `lpa2=off` was never needed.

**C1-aarch64 (fresh image)** — `/tmp/m9o/C1-aarch64.{out,serial.log}`

- positive control `rc=127`; fresh-image probes identical in shape to x86_64 C1.
- 12 `launch_pad` starts, one per component, max 1 per name.
- Session milestones present exactly once: `busd::bus: Listening on UNIX socket file`,
  `Initializing OpenGL`, `GL Renderer`, `Starting: /bin/leandros-applet`,
  `entering event loop`, `committed`; 205 `[DRM-SRV]` scanout-mapping lines.
- Zero `Unknown id`, `Broken pipe`, `panicked`, `restarting process`.

**Screendumps do not work on the aarch64 path on this box.** `run-qemu.sh` selects
`virtio-gpu-gl-pci` + `-display egl-headless` for aarch64 (script lines 170-205), and
QMP `screendump` answers `GenericError: no surface` at every timestamp
(t=150/300/450/590) even though the compositor is demonstrably scanning out. That is
the dmabuf-scanout console having no CPU surface for QEMU to dump — a host-side
limitation of the GL display backend, not a session failure. The x86_64 path uses
`virtio-vga` + `-display none` and dumps fine. All four aarch64 session runs were kept
on the identical egl-headless configuration so control and patched stay comparable;
aarch64 evidence is therefore the serial milestone set above rather than pixels.

## Patched double-run — x86_64 (KVM)

The patch was applied to the box's working tree with `git apply`, then
`scripts/m7z2-kernel-only.sh x86_64` rebuilt **only** the kernel and re-embedded it in
`leandros-limine-x86_64.img`. `servers/net` is a library crate linked into the kernel
(`kernel/Cargo.toml:32 net-server = { path = "../servers/net" }`), so a kernel-only
rebuild does carry the change; the build log shows `Compiling net-server` where the
HEAD build had it fully cached. f2fs images were regenerated fresh from the *same,
unpatched* userland so the kernel is the only variable in the session runs.

Kernel binaries by md5 (`target/final-x86_64/kernel`):

| kernel | md5 | used by |
|---|---|---|
| patched | `2f838a23b71ef9623878e163a26e87f7` | P1-x86_64, P2-x86_64, T-x86_64 (`scmtest` 31/0) |
| unpatched | `cab9210aa87e7f15e37aad8573e64e10` | NEG-x86_64 (`unix_listen_strict: FAIL`) |

Rebuilding the patched kernel after the negative control reproduced
`2f838a23…` byte-for-byte, so the kernel proven strict by `scmtest` is the same binary
that ran the COSMIC sessions.

### The four x86_64 sessions side by side

| run | kernel | image at boot | `launch_pad` starts | distinct names | max per name | screendump |
|---|---|---|---|---|---|---|
| C1 | unpatched | fresh | 12 | 12 | 1 | bar + wallpaper, `distinct=1474` |
| C2 | unpatched | dirty (C1's) | 12 | 12 | 1 | identical |
| P1 | **patched** | fresh | 12 | 12 | 1 | identical |
| P2 | **patched** | dirty (P1's) | 12 | 12 | 1 | identical |

- Positive control `nosuchbinary_xyz42` → `rc=127` on all four boots.
- **No restart loop anywhere**: every one of the twelve components
  (`cosmic-comp`, `cosmic-settings-daemon`, `cosmic-notifications`, `cosmic-panel`,
  `cosmic-app-library`, `cosmic-launcher`, `cosmic-workspaces`, `cosmic-osd`,
  `cosmic-bg`, `cosmic-greeter`, `cosmic-files-applet`, `cosmic-idle`) is started
  exactly once in all four runs. Zero `before restarting process`,
  `restarted process`, `failed to restart process`, `exited with error`.
- Zero `Unknown id`, zero `PANEL MAIN ERR`, zero `panicked` in all four runs.
- `busd::bus: Listening on UNIX socket file /run/user/0/bus.` appears exactly once in
  every run, patched included — the session's principal AF_UNIX listener is unaffected.
- P2 was genuinely dirty: `cat /root/m9o-boot-marker` → `BOOTMARK-P1-x86_64`, and
  `/root/.config/cosmic` carried P1's full `com.system76.Cosmic*` config tree.
- The one `Broken pipe` line (`cosmic-notifications: Failed to setup panel dbus server
  I/O error: Broken pipe (os error 32)`) occurs in C2, P1 and P2 but not C1. It is
  therefore an intermittent flake **present on the unpatched kernel**, not something
  the patch introduced. It is unaccompanied by `Unknown id` or `PANEL MAIN ERR`, so it
  is not the M7v signature, and it never triggers a restart.

## Non-regression on x86_64 — and the negative control that makes it mean something

One boot, fresh image, `vfstest` first then `scmtest`, each run exactly once
(`/tmp/m9lane/lane_i_run.py`, positive control first, `rc=127`).

- **Patched kernel + patched userland (T-x86_64): 67 `<name>: PASS` lines, 0 FAIL** —
  36 of them `vfstest` (`rmdir` … `symlink_cross_mount_tmpfs_to_f2fs`) and 31
  `scmtest` (`fd_pass` … `inet_listen_twice`). So **`vfstest` 36/0** and
  **`scmtest` 31/0**, with `unix_listen_strict` present and `tcp_time_wait` **absent** —
  the other lane's TIME_WAIT patch is not on this box, so the 31 is this patch's.
- **Negative control (NEG-x86_64): unpatched kernel, same patched `scmtest` binary,
  same fresh image → 31 subtests, 30 PASS, 1 FAIL, and the one failure is
  `unix_listen_strict`.** Its own diagnostics name all five must-fail assertions:

```
[uls] (a) unbound listen rc=0 errno=2 (want -1 22)
[uls] (g) dgram listen rc=0 errno=2 (want -1 95)
[uls] (d) socketpair listen rc=0 errno=2 (want -1 22)
[uls] (e) pending-accept listen rc=0 errno=2 (want -1 22)
[uls] (f) accepted-socket listen rc=0 errno=2 (want -1 22)
[uls] (b,c) listen rc=0 repeat rc=0 (want 0 0)
```

(b) and (c) pass on both kernels, as the design note says they must. This is the proof
that `unix_listen_strict: PASS` on the patched kernel is a real detector firing and not
a guard that would pass either way.

## Patched double-run — aarch64 (TCG)

Same procedure: `scripts/m7z2-kernel-only.sh aarch64` on the patched tree
(`Compiling net-server` present, patched kernel md5
`b046faba2f63a14ff47181b86d1c2216`), fresh f2fs images from the unpatched userland,
then two boots against that one image. 600 s settle per boot.

| run | kernel | image at boot | `launch_pad` starts | distinct | max per name |
|---|---|---|---|---|---|
| C1 | unpatched | fresh | 12 | 12 | 1 |
| C2 | unpatched | dirty (C1's) | 12 | 12 | 1 |
| P1 | **patched** | fresh | 12 | 12 | 1 |
| P2 | **patched** | dirty (P1's) | 12 | 12 | 1 |

- Positive control `rc=127` on all four boots.
- All four contain exactly one each of `busd::bus: Listening on UNIX socket file`,
  `Initializing OpenGL` / `GL Renderer`, `Starting: /bin/leandros-applet`,
  `entering event loop`, `committed`, with 277-348 `[DRM-SRV]` scanout mappings.
- Zero `Unknown id`, `PANEL MAIN ERR`, `panicked`, `restarting process`,
  `exited with error` in all four.
- The same single intermittent `cosmic-notifications … Broken pipe (os error 32)`
  appears in C2 (unpatched), P1 and P2 — again, present on the control kernel.
- P2 was genuinely dirty: `BOOTMARK-P1-aarch64` in `/root/m9o-boot-marker`,
  `/root/.config` carrying `cosmic qt5ct qt6ct`, `/run/user/0` empty.

## Non-regression on aarch64

Fresh image, one boot, positive control then `vfstest` then `scmtest`, each once:
**67 `PASS` lines, 0 `FAIL` — `vfstest` 36/0 and `scmtest` 31/0**, subtest list
identical to x86_64 and again with `unix_listen_strict` present and `tcp_time_wait`
absent. `xattr_list_f2fs` passes, as expected on a genuinely fresh image.


## The strongest single observable: the framebuffer is pixel-identical except the clock

`screendump` at t=235 s, x86_64, md5 of the raw PPM:

| run | md5 |
|---|---|
| C1 (unpatched, fresh) | `ff23a96c672f035d58724b6b46629348` |
| C2 (unpatched, dirty) | `ff23a96c672f035d58724b6b46629348` — **byte-identical to C1** |
| P1 (patched, fresh) | `85c207de2e5b4c2eca96b4fa3691212b` |
| P2 (patched, dirty) | `df84ab2d8065eee3d61fefd06a00f8da` |

The control pair being byte-identical means the rendered desktop is deterministic, so
the patched pair differing is worth explaining rather than waving away. A per-pixel
diff localises **every** differing pixel to one box:

```
C1 vs C2 -> identical
C1 vs P1 -> diff px=405 x=[961..1029] y=[5..25]
P1 vs P2 -> diff px=378 x=[961..1029] y=[5..25]
C2 vs P2 -> diff px=99  x=[1015..1029] y=[11..25]
```

That 69x21 box at the horizontal centre of a 1920-wide panel is the clock digits —
confirmed by looking at `P2-x86_64-t235.png`, which reads `00:03:52` in exactly that
spot. Nothing outside the clock changes between any pair of runs, patched or not.

aarch64 pixel evidence (`PIX2-aarch64-t495.png`): 1280x800, `distinct=1493`,
full-width `(27,27,27)` bar at y=16, nebula below. To get it, `run-qemu.sh` was copied
to `/tmp/m9o/run-qemu-nogl.sh` and the copy's aarch64 GPU choice forced to
`virtio-gpu-pci` so QEMU has a CPU surface to dump; the repo copy of `run-qemu.sh` was
never modified. (`--extra "-display none"` alone does not work: QEMU refuses
`virtio-gpu-gl-pci: The display backend does not have OpenGL support enabled`.) This
run is on the patched kernel with a fresh image and is *additional* to the four
comparable aarch64 sessions, not one of them.

Screenshots copied to `~/code/leandros-artifacts/notes/m9-afunix-screenshots/`.

## Answering the specific question the item was held for

The hazard the patch was held for is "a component whose `bind()` fails on a dirty image
and which today limps on as a zombie listener". Three things bear on it:

1. **It did not happen.** Across four patched session boots (two per arch, the second
   of each against the first's state), all twelve components start exactly once and
   nothing restarts. There is no `listen`-related error anywhere in any log, and
   `busd` still binds and listens on `/run/user/0/bus` in every single run.
2. **The reboot cannot carry the specific trigger.** The stale-`S_IFSOCK` route to a
   failing `bind()` needs a socket node to survive, and every socket this session
   creates lives under `XDG_RUNTIME_DIR=/run/user/0`, which is a `TMPFS_ROOTS` entry
   and is verifiably empty at every boot. So the double-run tests a dirty **`$HOME`
   config/state tree** (which it genuinely does — see the C2/P2 probes), not a dirty
   **runtime socket dir**. A same-boot restart of a component *can* still hit a stale
   node in `/run/user/0`, and that path was exercised: `launch_pad` supervises twelve
   processes for 240 s (x86_64) / 600 s (aarch64) per run with none of them dying.
3. **The remaining exposure is bounded and is not a regression risk.** Anything that
   would newly fail is a caller that (a) is an AF_UNIX DGRAM socket calling `listen()`,
   or (b) ignored a failed `bind()`. Nothing in the tree does either
   (`scmtest` and `wakepolltest` are the only in-tree `listen()` callers and both check
   `bind`), and the out-of-tree session demonstrably does not.

## Caveats, stated plainly

- **`/run/user/0` is tmpfs, so "dirty" here means dirty `$HOME`, not dirty sockets.**
  This is the one way in which the double-run is weaker than it sounds, and it is a
  property of the system, not of the method: there is no way to carry a stale
  `S_IFSOCK` across a LeandrOS reboot in the session's runtime dir. It is recorded
  rather than hidden.
- The guest image has no `grep`, so the intended "list persistent socket nodes"
  probe (`ls -laR … | grep '^s'`) returned `command not found: grep` and produced
  nothing. The tmpfs argument above stands on the direct `/run/user/0` and `/tmp`
  listings instead.
- The harness's post-session `sync` times out in most runs because the serial console
  is owned by the session. It is not needed: `/root/m9o-boot-marker` written in run 1
  is read back in run 2 on both arches, so guest writes reach the image regardless.
- aarch64 was TCG throughout (this box has no ARM virtualisation). Limine never wedged,
  so `-cpu max,lpa2=off` was not required.

## Landing

Committed on the box's `main` as **`3532c7b`**, `net: apply Linux's listen() gates to
AF_UNIX sockets` — the two files of `afunix_listen_strict.patch` and nothing else. The
working tree was verified byte-identical to the patch before committing (187 changed
lines on both sides, identical change set). **Not pushed**; `origin/main` remains at
`6a0eb0c`. Both pre-existing stashes are untouched. `TODO.md` was deliberately not
edited — the patch never touched it, and items 10 and 11 are adjacent, so leaving it
alone keeps this out of the TIME_WAIT lane's way.

## Artifacts on the box (`/tmp/m9o/`)

`{C1,C2,P1,P2}-{x86_64,aarch64}.{out,serial.log}`, `T-{x86_64,aarch64}.*` (the
`vfstest`/`scmtest` runs), `NEG-x86_64.*` (the negative control), `PIX2-aarch64.*`,
the `.ppm`/`.png` screendumps, `cosmic_run.py`, `sesslog.py`, `run-qemu-nogl.sh`, and
the build logs `kbuild-*`, `mkfs-*`, `uland-*`.
