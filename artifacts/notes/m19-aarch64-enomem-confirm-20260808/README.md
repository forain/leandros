# aarch64 confirms the port-table fix, and reproduces the failure at the same byte

2026-08-08, Mac (`/Users/forain/code/leandros`), **aarch64/HVF**, QEMU 11.x,
release builds, freshly generated f2fs images, `LEANDROS_QEMU_MEM` at its 2G
default. The x86_64/KVM half is
[m18-enomem-port-table-20260808](../m18-enomem-port-table-20260808/README.md),
which closed with its own scope limit stated plainly: *"everything above is
x86_64/KVM. aarch64 is unconfirmed"*. This closes it.

Harnesses: `artifacts/m19_a64.py` (session + a dense clock series, on top of the
inherited `m17_census.py`), `artifacts/m19_mutate.sh` (the falsification),
`artifacts/m19_greeter.py` (the graphical login), `artifacts/m19_regress.py`.
`nosuchbinary_xyz42` was confirmed **failing** as the first command of every
boot in this note.

## The answer

**aarch64 agrees with x86_64 on every load-bearing claim, and reproduces the
failure down to the same faulting address.** Two differences are worth having,
and neither weakens the finding — one is a *stronger* piece of evidence than
x86_64 could produce, and the other is a different shape of the same death.

## Before anything: the binary under test

`ports/busd/build.sh` applies its patches only when it first extracts the crate,
so the moved `service-unknown-reply.patch` needs `rm -rf ports/busd/.work`. The
staged aarch64 binary went

    md5 045e61f0b3b5f4f3dc43966c1cbe5047  ->  60c5f82cdf1f0ceaafbb5131b33a065a
    mtime 08:34                           ->  15:24

and the image build then reports `Packed busd (size: 2275768 bytes)`, which is
the new product's size to the byte. Both patches are in the extracted tree
(`ServiceUnknown` x3 in `src/peers.rs`, `#[tokio::main(flavor = "current_thread")]`
in `src/bin/busd.rs`).

**A staging gap on this machine had to be closed first, and it is worth
recording because it makes a committed file invisible.** `scripts/mkfs-f2fs-populated.py`
stages the guest half of the census harness from
`~/code/leandros-artifacts/m6-session-data/m17-census`, but the file is
*committed* at `artifacts/m6-session-data/m17-census`. The artifacts tree is not
the repo, the merge that brought the commit here did not populate it, and the
staging is conditional on `os.path.exists` — so the image was built without the
script and the only symptom was brush answering `failed to source file:
/bin/m17-census`. Everything else in that directory matched byte for byte except
`m4-vkwl`, which has a known per-machine variant. Copying the one file fixed it.

## 1. Does the desktop come up clean on aarch64 with both changes in?

Yes, on every axis, and the series says so rather than a single frame.

| measure | aarch64 (run 1) |
|---|---|
| panel bar | present from t+22 s, every frame after |
| clock | **22 distinct hashes of 22 frames** — `VERDICT: TICKING` |
| `[EXC] EL0 Fault!` | **0** |
| `Out of memory (os error 12)` | **0** |
| `[IPC] port table FULL` | **0** |
| `[VFS] ENOMEM: no reply port` | **0** |
| `[SCHED] task table FULL` | **0** |
| free RAM | 1103-1935 MiB at every sample, trough 1103 MiB |
| `procs` at settle | 19 |

The tick series is the 220x32 band at the top centre of the scanout — the crop
the M9c clock lane used, so a FROZEN verdict here would have been comparable
with the one recorded there. All 22 frames differ. The whole-frame diff boxes
across the settle windows are confined to roughly `(680, 8)-(708, 24)`: nothing
on the screen changes except the clock, which is what a live idle desktop looks
like.

## 2. The four parked components come alive, and one of them is photographed

`cosmic-launcher`, `cosmic-app-library`, `cosmic-workspaces` and `cosmic-osd`
had been stuck in libcosmic's blocking D-Bus single-instance probe every boot
since they were staged. The census names all four:

```
  1  com.system76.CosmicAppLibrary
  1  com.system76.CosmicLauncher
  1  com.system76.CosmicOnScreenDisplay
  1  com.system76.CosmicWorkspaces
```

— each addressed once as an unowned name, which is precisely the call the
`ServiceUnknown` reply answers instead of dropping. What they do afterwards:

| component | evidence it is running |
|---|---|
| `cosmic-launcher` | hand-started copy exits with **"Another instance is running"** after **"Successfully activated another instance"** — the autostarted copy owns the name, which is only reachable through the probe. It then reports `ERROR pop-launcher failed to start: No such file or directory (os error 2)` |
| `cosmic-workspaces` | reaches `zbus::object_server dispatch_call`, **and draws** |
| `cosmic-app-library` | reaches `zbus::object_server dispatch_call` and `iced_winit …wayland::event_loop` |
| `cosmic-osd` | reaches `zbus::object_server dispatch_call` |

`pop-launcher` is the expected caveat, not a failure: the launcher spawns it by
bare name through PATH and it is neither built nor staged. The line is *positive*
evidence — a component blocked in the single-instance probe never gets far enough
to start its backend and complain about it.

**The x86_64 half could only say "a component drew a window and kept it", from a
coverage number. Here the component is identifiable.** `run1-control-probe-t45.png`
and `run1-control-probe-t84.png` are the workspaces overview: a bordered
"Workspace 1" tile with a live thumbnail of the desktop inside it, drawn down the
left of the screen. It is still there 39 s later, unchanged, with the panel clock
having advanced `00:02:56 -> 00:03:31`. Non-background coverage moves
`0.961 -> 0.177` at `probe-t4`, settles at `0.821` by `probe-t45` and **stays**
through `probe-t84`, where the only pixels that differ are the clock.

The direction of that coverage change is the opposite of x86_64's
`0.971 -> 0.469`, because a different component ended up on top; the persistence
is the claim, and it holds.

## 3. Falsification — aarch64 reproduces, at the same address

`artifacts/m19_mutate.sh`. `LIVE_BUCKETS` back to 64 with the **kernel as the
only delta**: the same staged busd binary, the same image regenerated from the
same inputs, the same harness, the same phase timings.

```
control  f9d62db947bba2af3a0cb4908f73a148
mutant   2a6bd55bcdfcbe41f953ec200a459d44
restore  f9d62db947bba2af3a0cb4908f73a148     <- byte-identical to the control
image    0d26fc2a0ef1d5c90568cdf9fde1b219
```

The control md5 is doubly anchored: `scripts/m7z2-kernel-only.sh` reproduced the
exact binary `scripts/build-all.sh` had already produced and the control session
had already run, so "control", "restore" and "the kernel that was measured" are
one file.

The chain, in one boot, on one console — lines 14 / 17 / 22 of
`run2-mutant-64-serial.log`, against x86_64's lines 88 / 91 / 98:

```
[IPC] port table FULL: 64/64 live buckets -- port::create now fails, and every caller
[IPC] turns that into ENOMEM; this is ipc::port::LIVE_BUCKETS, not RAM

[VFS] ENOMEM: no reply port for this task -- every call to a mounted
[VFS] filesystem (open/stat/getdents64/exec) now returns errno 12. Not RAM.

[EXC] EL0 Fault! PID=126 ESR=0000000092000006 FAR=0000000000000880 EC=0000000000000024 DFSC=0000000000000006
```

`FAR=0x880` is x86_64's `CR2=0x880` — the same read of `ctx` at offset `0x880`
inside libxkbcommon, which is a **struct offset** and therefore architecture-
independent. `ESR EC=0x24 / DFSC=0x6` is a data abort from a lower EL on a level-2
translation fault: a read of an unmapped low address, i.e. the null dereference.
Four EL0 faults in total; the session then wedged and never emitted its second
phase marker.

**The one real difference.** On x86_64 the mutant had *no panel bar at all*. On
aarch64 the panel is drawn — once — and then the entire 1280x800 frame goes
byte-identical (`diff_vs_prev=None`, the same band hash at t+22 s and t+39 s).
`run2-mutant-settle1-t39.png` shows why that is the same finding and not a weaker
one: the clock reads **`00:00:09`** at forty-odd seconds of guest uptime. The
compositor got one frame out at the moment the table filled and nothing has
redrawn since, because the tasks that would redraw are being killed. Absent
panel and frozen panel are two renderings of one death; a single screenshot would
have called the aarch64 mutant a **pass**, which is exactly why the standard is a
series.

## 4. The graphical login still reaches a desktop

`artifacts/m19_greeter.py`, real greetd behind cosmic-comp's kiosk mode, password
typed on the virtio-keyboard through the monitor's `sendkey` (genuine guest
input).

* `run3-greeter-login-screen.png` — the greeter, with `leandro` as the **only**
  offered account (the UID_MIN/UID_MAX filter working: root at 0 and
  cosmic-greeter at 990 are both correctly absent) and a focused password field.
* the serial log then shows `INFO cosmic_session: Starting cosmic-session` at
  guest `00:00:43`, about two seconds after `ret`.
* `run3-greeter-desktop-after-login.png` — **the desktop**, wallpaper and panel,
  clock reading `00:01:56`.

Coverage goes `0.246` (greeter) -> `~0.13` (black, session restarting) ->
`0.961` (desktop) and holds it for the rest of the sample, with a different panel
band hash in every desktop frame. **0** faults, **0** `Out of memory`, **0**
`port table FULL` for the whole login.

The greeter does not echo anything into the password field as it is typed, so the
frame captured immediately after typing is identical to the one before it. That
is a rendering choice, not a lost keystroke — the authentication that follows is
the proof the characters arrived.

## 5. Regression, aarch64, fresh image, `vfstest` once

| binary | result |
|---|---|
| `vfstest` | **0 FAIL**, `--- vfstest done ---` (no trailer; FAILs counted) |
| `scmtest`, `wakepolltest`, `forktest`, `epolltest`, `polltest`, `sigtest`, `timertest`, `memtest` | **0 FAIL** each, own `done` marker present |
| `waittest` | **0 FAIL** — `wait_on_process_group` did not flake this run |
| `venustest` | 32 FAIL — **the device does not exist on this host** |
| `[EXC] EL0 Fault!` over the whole suite | **0** |

`vfstest` includes `xattr_list_f2fs: PASS`, consistent with that long-standing
"known aarch64 red" being a dirty-image artifact rather than a kernel bug.

`venustest`'s 32 is a host-capability artifact and is stated precisely because it
arrives in the shape of a regression. It is not merely that `--venus` was not
passed: `qemu-system-aarch64 -device help` on this Mac lists **only**
`virtio-gpu-device` and `virtio-gpu-pci` out of 368 devices — there is no `-gl`
variant, because macOS has no EGL for virglrenderer to build against. Every
failure is `host_advertises_venus_capset` and its dependents plus the three
`getparam_*` probes that need a 3D-capable device. Venus on this host is
impossible, not broken.

**x86_64 was affordable, so it was run too** (`run5-regression-x86_64-harness.txt`),
against an image rebuilt on this machine with the newly staged x86_64 busd
(`Packed busd (size: 2475480 bytes)`). Same result: every binary 0 FAIL with its
own `done` marker, `user page fault … task killed` **0** over the whole suite, and
`venustest` 32 for the same host reason — `qemu-system-x86_64 -device help` has no
`-gl` variant either. So the numbers the Linux box reported for x86_64 hold on this
host as well, with the one difference being Venus, which is the host and not the
build.

The M18 reproducer run against the **fixed** kernel is its own control here too:
the same 60-job burst stops at brush's descriptor limit
(`No file descriptors available (os error 24)`), never errno 12, and the kernel
names **no** ceiling — `port table FULL`, `fd-table pool FULL`, `task table FULL`
and `no reply port for this task` are all 0.

### Re-run after the staging fallback landed

Making `session_data()` fall back to the committed copy changes what this machine
puts in the image: `m12-caps` and `m12c-input` were in the repo but not in the
artifacts tree, so they are now staged where before they were silently skipped.
That is additive — two guest driver scripts in `/bin`, no existing file changed —
but it is still a change to the image builder made *after* the numbers above were
taken, so the aarch64 suite was run again on a freshly generated image
(`run6-regression-aarch64-after-mkfs-fallback.txt`). All eleven binaries read back
identically: 0 FAIL each with its own `done` marker, `venustest` 32 for the same
absent-device reason. **The reproducer tail of that re-run was still executing when
the session was wound down, so it is the one thing here not read back**; the same
reproducer against the same kernel had already answered errno 24 twice.

The fallback itself was verified directly, by hiding
`~/code/leandros-artifacts/m6-session-data/m17-census` and confirming the image
still reports `Packed m17-census (size: 6145 bytes)` from the repo copy.

## What is x86_64-only, and what is not

Nothing load-bearing is x86_64-only. The list of things that differ:

1. **The mutant's panel is frozen rather than absent** (section 3). Same death,
   different rendering, and the one that a single capture would misread.
2. **The probe coverage moves the other way** — `0.961 -> 0.821` here against
   `0.971 -> 0.469` there — because a different component ends up on top. The
   claim that survives on both is *persistence*, not direction.
3. **Venus cannot be measured on this host at all**, where the Linux box could
   score `failures = 0`. That is the host, not the arch.
4. `-m 4G` was not touched, per its standing separate bug in `mm::buddy::free`.

## What surprised

1. **The falsification's aarch64 signature would pass a one-frame test.** A
   correctly drawn panel bar, correct wallpaper, correct geometry — and a clock
   stopped at `00:00:09`. The only instrument that catches it is the series.
2. **aarch64 produced better evidence for the components than x86_64 did.** The
   x86_64 half inferred "a component drew a window" from a coverage delta; here
   the window is legibly `cosmic-workspaces`, with a live thumbnail in it.
3. **A committed file can be missing from the image.** The census script is in
   the repo and was still absent from the guest, because the image builder reads
   an unversioned sibling tree and skips silently when the file is not there.
