# LeandrOS — TODO

Remaining work only. Finished items are deleted, not marked done; `git log` is the record.
Reconciled against `main` at `09f70832` (2026-11-11). Per-lane evidence: `artifacts/notes/lane-<name>-<date>.md`.
Items marked (?) were not re-verified since the 2026-10 reconciliation.

## Known bugs

### Kernel / VFS
- **Podman rootless pasta churn**: earlier 3/4/1 of 20 failures, unverified after the fcntl lock fix (?).
- **Pending AF_UNIX connections whose client closed before `accept` are dropped** (Linux hands them over) (?).
- **Paths below `/proc/self/fd/N` follow the directory's open-time path** (no dentries; no " (deleted)" suffix on readlink).
- **Global scans on last close** of tmpfs/mounted files walk every fd table (O(all fds)); a per-object refcount would retire it and `FD_COPY_GATE`.
- **AF_UNIX connection pool (512) is a fixed system-wide pool** (?).
- **Wake scans are O(parked pollers)**; keyed wake still dereferences each mask match (~11 us per scan). A per-tag index / kbits bloom would fix it.
- **The switch-back CPU charge still takes RUN_QUEUE** (~22.5k holds/s); needs a lock-free `on_cpu` release protocol.
- **New writers of signal state must call `sig_kick`**; a miss shows as `miss` > 0 in `/proc/rqhold` (invariant, no known violation).
- **Flakes to root-cause**: `epolltest unrelated_unix_traffic_no_wake` (11% vs 1/8 threshold in a busy session), `sigtest` (?), `mnttest` tfs_* ordering.
- **Page cache**: dirty `MAP_SHARED` pages stay dirty while mapped writable (no rmap); global `CACHE` lock contention unmeasured.
- **F2FS fsync is durable only via full checkpoint** (no fsync node log, no NAT/SIT journal); `f2fs.fsync=lazy` trades a ~5 s loss window.
- **Buddy-slack leak on the ELF loader's eager-to-lazy split** (?); lazy-fault residual ~27 pages/death.
- **Interrupt-context serial diagnostics can drop under back-pressure** (by design).
- **fd-limit fence loss**: EMFILE in a virgl client's submit kills its context (as Linux); fd headroom is the defence.
- **Host-refused `RING_IDX` submit is never completed by QEMU** (descriptors lost); Mesa cannot reach it. Do not rewrite RING_IDX kernel-side.
- **Pty background-child SIGHUP on su session end** unverified against Linux (?).

### Podman / namespaces / cgroups
- **`memfd` exe pin not inherited by fork** (?).
- **nsfs bind inside a non-init mntns; self-binds not copied into a new mntns.**
- **netns**: SETLINK move of an existing link; TCP over a tap in a netns (?).
- **fchmod/fchown/futimens on fds of an ro mount** are not refused.
- **net/ipc/uts namespaces share one slot-kind class** (LOW; fold into per-kind).
- **mntunshare is not an isolation boundary for root-run workloads** (M6).
- **rename on devfs unsupported.**
- **Redundant `local_loop` case in podpasta** (duplicate of `loop_own`); podpastatest `bridge_ip` has no httpd in Alpine (now an nc loop) and leaks a baseline mount.
- **cgsession: only `/bin/login` joins the session cgroup** (greetd/sshd/launcher/autologin need a helper).

### Network
- **No IPv6 / AF_INET6** (sshd `[::]:22` fails with EAFNOSUPPORT; podman IPv6 deferred).
- **UDP/IP gaps**: no PMTUD, no IGMPv3/SSM, no IP fragmentation/reassembly (?), `connect(AF_UNSPEC)` quirks (UDP keeps bind(0) port; TCP takes the AF_UNIX path).
- **Real NIC drivers program no multicast filter** (mDNS works only in QEMU).
- **Inet listeners report no tag**, so sshd parks with a broadcast mask.
- **iwlwifi gaps**: later-beacon WMM changes ignored, STATISTICS_NOTIFICATION ignored, antennas only toggled on failure; TX MIMO/STBC/short GI on 5 GHz untested; 2.4 GHz poor (36% AP retries); sparse pings lose 1-4%.
- **Laptop SSH came up late on two 2026-10 boots** (not reproduced in ten later boots; watch).
- **`netwatch.loss=S` not exercised in QEMU.**
- **Podman published ports**: TCP NAT port reservation / ICMP error translation open; rootful `-p`, `--internal`, host-NIC reach deferred.
- **r8169 RX stalls/chip resets on the PC**: ISR-mask fix needs PC confirmation (R8169 as a loadable module also deferred).

### Desktop / GPU / COSMIC
- **Venus/Vulkan**: ISR path unwired, virtgpu `FENCE_FD_IN`/`in_syncobjs`/`out_syncobjs` not honoured, cross-open dmabuf import deferred (`artifacts/notes/m9-crossopen-dmabuf/`), aarch64 vkwl untested.
- **i915**: cursor 64x64 only, no async flips, no real `WAIT_VBLANK`, no `IN_FENCE_FD`.
- **Laptop**: low FPS under COSMIC; Firefox on iris unproven; Wi-Fi link still weak on 2.4 GHz.
- **x86 audio clipping** in DOOM/PipeWire (lane/doomvis).
- **PipeWire sink is named "Virtio Sound Output"** whatever the backend (HDA on the PC shows that name).
- **Minimize applet un-hide unverified**; `cosmic-files-applet` smithay-clipboard OOM panic and `cosmic-notifications` panel-dbus Broken pipe have no root cause; greeter's cosmic-comp logs `eglMakeCurrent BadDisplay` on exit (cosmetic).
- **Panel-embedded applets render with tiny-skia**; aarch64 desktop clicks need a physical-mouse check (P0 #5).
- **VT switching**: a present inside its handler during a switch can flash; evdev VT gate not tested against cosmic-comp; `EVIOCGRAB` exclusive delivery unmeasured; `EVIOCREVOKE` ignored; Ctrl+Alt+Esc swallowed.
- **One permanent COSMIC patch** (`ports/cosmic-comp/0001-toplevel-info-weak-window.patch`) until upstream has an equivalent.
- **x86_64/TCG COSMIC settle timing is stale** — re-measure (clock fixed 2026-09-16).

### Hardware (laptop / PC / Pi)
- **Stick write commands of 0.5-1.3 s are hidden, not removed** (USB hubs now supported, usbhot2).
- **Why Limine leaves RAM above 4 GiB out of the HHDM is unexplained** (`lane-fbmm`); aarch64 has no HHDM gap walker.
- **Real-PC (RTX 3070) verification deferred ~1 month** (user, 2026-10-24).

### Build / tooling
- **Image can silently ship without `libseat.so.1`/`libudev.so.1`** (no zig, no blob cache): build succeeds, COSMIC fails at runtime. Same for other `os.path.exists()`-guarded blobs.
- **Fresh worktrees boot with no shell** unless `brush`/`coreutils`/`bottom-leandros` siblings resolve; build exits 0 with only a warning.
- **`../crossterm` sibling required by brush**; `/bin/wl-globals` staged only if the host binary exists.
- **Doom links `leandros-libc`** whose allocator is a bump allocator over `brk` (no free list).
- **A userland test built with bare `cargo build` lacks `-C relocation-model=static`** and faults before `main`.
- **`~/code/leandros-artifacts` is unversioned and diverges from `artifacts/`**; diff before trusting either.

## Missing features

### Kernel
- inotify L2/L3 (beyond the default-on L1).
- **Interrupt paths**: virtio keyboard/blk/net/snd and the aarch64 GPU control queue are still polled (INTx via GIC SPI is the aarch64 step).
- **Synthetic sysfs**: only `/sys/class/block` (+ cgroup/kernel paths); `/sys/dev/char`, `/sys/class/drm`, `/sys/class/input` per `docs/design/k4-drm-design.md` deferred.
- **utmpx/wtmp/lastlog**: unimplemented; establish a consumer (`who`, `w`, `last`) first.
- **Replace the C `libpam.so.0` shim** with a Rust cdylib or Linux-PAM (decision open; guard must show wrong passwords rejected).
- **VT console**: TAB advance and double-width glyphs.
- **sshd**: privilege separation, `authorized_keys` options, keyboard-interactive, forwarding, MaxStartups, utmp/lastlog; password hashes are single-pass salted SHA-256.

### Network / Wi-Fi
- **iwlwifi**: 40 MHz on 2.4 GHz, finer rate control.
- **WPA3 / MFP-required networks** and Wi-Fi roaming polish.

### Desktop / apps
- **xdg-desktop-portal** (screenshot, file chooser), timedate1, SwitcherooControl and the a11y bus D-Bus services.
- **`cosmic-store` backend** (PackageKit service or libflatpak); **`cosmic-player`** (needs GStreamer closure, ~1-2 days).
- **A permanent Vulkan-Wayland test** (needs a dynamic-musl build shape).
- **VRR** (`VRR_ENABLED` DRM property).
- **Mesa v3d / Pi 5 acceleration** wiring (`GET_PARAM`+`CREATE_BO` first; single-device design).
- **XWayland, accountsservice, hotplug, multi-seat** are explicitly out of scope.

### Firefox
- **Firefox on the laptop iris GPU**; x86 KVM audio verification (?).
- **Sandbox follow-ups**: secirq, zaplog, credxlate residuals (`lane-secirq/zaplog/credxlate-2026-11-09.md`).

### Installer
- **Dual-boot**: no whole-disk wipes; Windows/NTFS and new-machine flows per `docs/design/dualboot-methodology.md`.

### Toolchain / dev experience
- **zig to clang** migration (est. 2-4 days, not started).
- **Claude Code in-guest**: `claude-run` only appends to `/etc/passwd`/`group`; only 2.1.296 is pinned; x86_64 VT/cosmic-term runs not screenshotted.
- **Test harness**: run with `python3 -u` and no pipe; pass `-D <file>` when host tracing is on.
