# The COSMIC desktop

LeandrOS boots, on both architectures, to a graphical login that hands off to a full
[COSMIC](https://github.com/pop-os/cosmic-epoch) desktop: wallpaper, a panel bar and a
dock with real applets, a launcher, Settings, a terminal emulator on a kernel PTY, D-Bus
activation, an xdg-desktop-portal, PipeWire audio, and Shut Down / Restart that really
power the machine off. COSMIC itself is built from the upstream `epoch-1.3.0` sources
with build-configuration flags only; everything beneath it — kernel, servers, libc
choices, shims, launchers and system services — is LeandrOS.

This section describes how the desktop is put together and why. The pages below it go
into the login path, the running session, and what still does not work.

| Page | What it covers |
|---|---|
| [login.md](login.md) | greetd + cosmic-greeter, `greeter-launch`, `pam-leandros`, `vt = "none"`, the socket directories, the renderer gate |
| [session.md](session.md) | `cosmic-session`, busd and D-Bus activation, `ServiceUnknown`, the system services, applets, pop-launcher, the portal, power-off |
| [limitations.md](limitations.md) | What the desktop still cannot do, and which gaps are recorded rather than designed |

## Overview

```
init (PID 1)
 ├─ serial getty (ttyS0) and text logins on VT 2..6
 └─ /bin/greeter-real ── greetd                                       [VT 1]
     ├─ greeter session (root):
     │    cosmic-comp (kiosk) → greeter-launch → cosmic-greeter        [uid 990]
     │                                               ↓ authenticates over greetd IPC
     └─ user session (the account that logged in):
          sh -c '. /etc/profile; exec start-cosmic'  →  dbus-run-session
            ├─ busd                D-Bus broker (pure Rust, from the zbus authors)
            │    └─ activated on demand: leandros-sysbus (login1, locale1, UPower,
            │       power-profiles, PolicyKit1, RealtimeKit1), xdg-desktop-portal
            │       + xdg-desktop-portal-cosmic + xdg-permission-store
            ├─ pipewire (+ WirePlumber, the virtio-snd sink, pipewire-pulse)
            └─ cosmic-session      launch-pad supervisor
                 ├─ cosmic-comp           Wayland compositor (Zink on Venus, or virgl)
                 ├─ cosmic-settings-daemon, cosmic-notifications
                 ├─ cosmic-panel          Panel + Dock, 8 applet processes
                 ├─ cosmic-bg, cosmic-osd, cosmic-launcher (+ pop-launcher),
                 │  cosmic-app-library, cosmic-workspaces, cosmic-files-applet, cosmic-idle
                 └─ on demand: cosmic-term, cosmic-settings, cosmic-randr, Firefox, …
```

The session processes run as the logged-in account: a login as `leandro` produces a
session where every `cosmic-*` process, the compositor included, reports `Uid: 1000` in
`/proc/<pid>/status`. Only the *greeter's* compositor stays root (see
[login.md](login.md#limitations--open-work)).

## How it works

### Committed architecture: dynamic musl

COSMIC is built for `*-unknown-linux-musl` and **dynamically linked** against a real
`ld-musl` shipped in the image (`/lib/ld-musl-<arch>.so.1`). `dlopen` sits on the critical
path in three separate places — cosmic-comp's EGL loading, Mesa's GBM/DRI loader, and
cosmic-panel's `use_system_lib` — so static linking was never an option. Supporting this
is what made the ELF loader grow real dynamic-linking support (`ET_DYN` loaded at a bias,
`PT_INTERP` honoured), VMA splitting at range boundaries for `munmap`/`mprotect`, an 8 MB
user stack matching the Linux default, and shared tmpfs/memfd pages for `wl_shm` pools
(see [../kernel/processes.md](../kernel/processes.md) and
[../kernel/memory.md](../kernel/memory.md)). The C-library side of that decision is in
[../userland/libc.md](../userland/libc.md#musl).

Each COSMIC binary is cross-built from the pinned `../cosmic-epoch` checkout with one
toolchain (`build-rust.sh` in the artifacts tree, `cargo +nightly`, musl targets, Rust
dependencies static, `DT_NEEDED` limited to the C libraries that really are shared:
`libc.so`, `libxkbcommon`, `libinput`, `libseat`, `libudev`, `libgbm`, `libpixman`,
`libdisplay-info`, plus whatever is `dlopen`ed at run time). `scripts/mkfs-f2fs-populated.py`
stages them into `/bin` under the exact names `cosmic-session` spawns them by (for example
`cosmic-app-library`, whose built file is `cosmic-applibrary`).

### Rendering: the GPU, verified, never a silent software fallback

The renderer is chosen per boot by `/bin/gpu-env`
([ports/greetd/data/gpu-env](../../ports/greetd/data/gpu-env)), which every launcher
sources: `/etc/profile` (both greetd sessions), `/bin/greeter-real` and
`start-cosmic-leandros`. It asks `/bin/gpuprobe` which virtio-gpu capsets card0 offers,
then creates a real GBM + EGL context exactly the way cosmic-comp will and reads
`GL_RENDERER` back. The order of preference is:

| Path | What it is | When |
|---|---|---|
| `zink` | OpenGL on Vulkan over the Venus capset: cosmic-comp's GLES becomes Vulkan work on the host GPU | `virtio-gpu` with `venus=on` and the Venus ICD + `libvulkan.so.1` in the image |
| `virgl` | OpenGL passthrough over the virgl capset | a host with GL but no Vulkan, the Mac's ANGLE/Metal path, UTM |
| `refused` | no hardware renderer | the launcher exits 78 (`EX_CONFIG`); init prints the reason once and stops respawning |

softpipe is reachable only by explicit opt-in (`LEANDROS_RENDERER=software`, a
`/etc/leandros/renderer` file containing `software`, or the marker
`/etc/leandros/allow-software-render`), and even then a banner says so in the log.
`LEANDROS_RENDERER=zink|virgl` forces one path, still verified.

The clients render on the GPU too. The libcosmic applications are built with libcosmic's
`wgpu` feature ([ports/cosmic-wgpu/build-wgpu-clients.sh](../../ports/cosmic-wgpu/build-wgpu-clients.sh),
feature flags only), and `gpu-env` exports `ICED_BACKEND=wgpu` plus
`WGPU_BACKEND=vulkan` (Zink path) or `gl` (virgl path). The CPU fallback (`wgpu,tiny-skia`)
is used only where wgpu cannot get a context — a host GL below GLES 3.1 or without four
SSBO blocks per stage, where `gpuprobe` reports `GL_WGPU=0` and a banner explains it.

The graphics side of this (Venus, virgl, Zink, Mesa, KMS) is documented in
[../graphics/README.md](../graphics/README.md).

### The process tree, and who starts what

- **init** supervises the graphical login next to the serial getty and the VT 2..6
  logins ([../userland/programs.md](../userland/programs.md#init)). greetd's whole
  chain writes to `/var/log/greetd.log` through a rotating logger (two 8 MiB
  generations), not to the framebuffer console the compositor is about to draw over.
- **greetd** runs the greeter session (cosmic-comp in kiosk mode with
  `greeter-launch` as its child) and, after a successful login, the user's session
  ([login.md](login.md)).
- **cosmic-session** is a [launch-pad](https://github.com/pop-os/launch-pad)
  `ProcessManager`, an in-process supervisor rather than systemd. It starts
  cosmic-comp first, blocks on a `socketpair` rendezvous until the compositor reports
  `WAYLAND_DISPLAY`, then starts the rest of the session by bare name through `PATH`
  ([session.md](session.md)).

## Design decisions

### Zero COSMIC source patches, with one approved exception

The project rule is that temporary debug edits to COSMIC are fine and permanent ones are
not: every permanent fix belongs on the LeandrOS side — kernel, shims, launchers or
staging. A COSMIC symptom is treated as evidence about LeandrOS until proved otherwise,
and that has been right far more often than not.

Two permanent patches once existed and both are retired:

- **cosmic-greeter** carried `0001-locker-idle-without-logind.patch`. It went by a
  staging decision instead: the binary is staged as `/bin/cosmic-greeter-login`, so
  `cosmic-session` cannot find it under the name it would spawn in the lock-screen role
  ([ports/cosmic-greeter/README.md](../../ports/cosmic-greeter/README.md)).
- **cosmic-session** carried `0001-env_rx-timeout-fallback.patch`, which raced the
  compositor handshake against a 5 s timeout. It worked around the kernel's `shutdown(2)`
  ignoring `how` (`2d9f0c8`) and poll wakes being broadcast to every poller (`daaf2cc`).
  With both kernel bugs fixed, the patch was deleted, the vendored source restored
  byte-identical to upstream, and `SetEnv` re-verified arriving on every boot. Measured
  payoff: the component cascade used to start 5.022 s after `Starting cosmic-session`
  (the 5 s timeout expiring, to within 22 ms) and now starts at 2.186 s
  ([ports/cosmic-session/README.md](../../ports/cosmic-session/README.md)).

The **one approved exception** is
[ports/cosmic-comp/0001-toplevel-info-weak-window.patch](../../ports/cosmic-comp/0001-toplevel-info-weak-window.patch),
approved on 2026-09-27 for one leak only. Each closed window left one GL texture alive in
cosmic-comp, because every `ZcosmicToplevelHandleV1` held a *strong* clone of its window
that `remove_toplevel()` never cleared. The patch is a port of the community `weak_window`
fix (Smithay#1562, cosmic-comp#2084): the handle stores `Window::Weak` and upgrades it on
use. Smithay's draft PR #1921 was tried first and refuted for this leak — the same ~1
texture per closed window persisted with it. Remove the port when upstream merges the fix.

Patches to software that is *not* COSMIC are a different matter and are made freely when
they are the right fix: busd (ours to ship, [session.md](session.md#busd)), greetd
([login.md](login.md#the-greetd-deltas)), brush, and two Mesa patches.

### GPU rendering only

COSMIC must render on the GPU; software rendering is never an acceptable target and never a
reason to patch COSMIC. The rule was set on 2026-09-25 after the greeter's keystroke lag was
fixed partly with a cosmic-greeter "blur off under softpipe" patch; the patch was reverted
the same day (`d16c563`) and the lag fixed properly instead: an f2fs block cache of 4 slots
(raised to 192) plus GPU rendering by default took per-key latency from a 35.5 s median to
0.168 s on x86_64/KVM ([lane-greeterlag](../../artifacts/notes/lane-greeterlag-2026-09-24.md)).
softpipe numbers are informational only; COSMIC performance is measured on the GPU path.

Before that, softpipe through gallium `kms_swrast` over dumb buffers was the default, and
the desktop composited at one frame every few seconds; Zink was opt-in behind `--venus`.
The history of that path, including the measurement that the early sluggishness was the
kernel and not softpipe, is in [../overview/history.md](../overview/history.md).

### Match Linux exactly, and let the desktop find the gaps

Most of the kernel work in this area was not "add a feature" but "match Linux exactly": a
full multithreaded GPU desktop is an extremely effective probe for compatibility gaps that
no test suite had surfaced. Roughly thirty real kernel bugs were found and fixed this way
during bring-up, and the desktop has kept finding them since. Among them:

- the `execve` signal-reset / de-thread trio (caught handlers surviving exec, sibling
  threads running against the new address space);
- TGID-keying of per-process tables — fd tables, `mmap` device detection, memfd ownership —
  looked up by thread id from worker threads (the softpipe `FAR=0x10` crash was one line in
  `vfs_get_node_kind`);
- dropped `eventfd` / `accept4` / `socketpair` / `F_SETFD` flags;
- a poll-deadline lost wake and a cross-thread timed-futex wake race;
- `AF_UNIX` refcounting, writes on connected-but-unaccepted sockets, and a full ring
  returning `EAGAIN` rather than 0;
- a `mincore` stub that returned success for unmapped pages, which made Mesa's
  `_eglPointerIsDereferenceable` accept `(void*)3` and fault the panel deep inside
  `eglCreatePlatformWindowSurfaceEXT`;
- private futexes keyed by address alone, so five instances of `cosmic-panel-button`
  starting in lockstep stole each other's wakes (`a19267d`, now keyed by tgid + address);
- `FUTEX_WAIT_BITSET`'s absolute deadline read as an interval, which stalled a compositor
  started after a greeter login for minutes per backoff step.

Each of these has a regression test in `userland/` (see
[../development/testing.md](../development/testing.md)).

### A COSMIC symptom is four different states

Four states look identical on a blank screen and imply completely different work: **absent
from the image**, **staged but never launched**, **launched but crashes**, and **runs but
renders nothing**. The desktop bring-up was misled repeatedly by conflating them — an
"applet-count ceiling" that was really one listening socket failing to `dup`, a "frozen
panel" that had no content to draw, "libcosmic cannot render" that was a D-Bus probe
blocking before the first frame. Investigations here start by establishing which of the
four a component is in.

## Limitations & open work

Summarised here; the full list with causes is [limitations.md](limitations.md).

- greetd uses no PAM stack and records no utmpx; the greeter's own compositor runs as root.
- No document portal (no FUSE), no ScreenCast through the portal; some system services
  (timedate1, SwitcherooControl, the a11y bus) are absent.
- A use-after-free in iced's SCTK thread can crash `cosmic-term` on exit (upstream; Linux
  has it too).

## Code & tests

| Where | What |
|---|---|
| [ports/greetd/](../../ports/greetd/) | greetd recipe, patches, `pam-leandros`, `greetd.conf`, `/etc/profile`, `gpu-env`, the greeter launchers |
| [userland/greeter-launch/](../../userland/greeter-launch/) | the greeter's privilege drop |
| [artifacts/m6-session-data/start-cosmic-leandros](../../artifacts/m6-session-data/start-cosmic-leandros) | the session launcher (POSIX sh, brush-safe) |
| [ports/busd/](../../ports/busd/), [ports/dbus/session-pkg/](../../ports/dbus/session-pkg/) | the broker, its patches, `session.conf`, the `.service` files |
| [ports/sysbus/](../../ports/sysbus/) | login1, locale1, UPower, power-profiles, PolicyKit1, RealtimeKit1 |
| [ports/portal/](../../ports/portal/) | xdg-desktop-portal + the COSMIC backend |
| [ports/cosmic-comp/](../../ports/cosmic-comp/), [ports/cosmic-session/](../../ports/cosmic-session/), [ports/cosmic-greeter/](../../ports/cosmic-greeter/), [ports/cosmic-wgpu/](../../ports/cosmic-wgpu/), [ports/pop-launcher/](../../ports/pop-launcher/) | COSMIC build recipes |
| [scripts/mkfs-f2fs-populated.py](../../scripts/mkfs-f2fs-populated.py) | what lands in the image, and the long comments on why |

## See also

- [../graphics/README.md](../graphics/README.md) — the GPU paths the desktop renders on
- [../input.md](../input.md) — evdev, libinput, the libseat/libudev shims
- [../servers/tty-and-vt.md](../servers/tty-and-vt.md) — virtual terminals and per-VT DRM master
- [../audio.md](../audio.md) — PipeWire in the session
- [../getting-started/logging-in.md](../getting-started/logging-in.md) — accounts and how to log in
- [../userland/ports.md](../userland/ports.md) — every port and its recipe
