# LeandrOS — TODO

Remaining work only. Finished items are deleted, not marked done; `git log` is the record.
Reconciled against `main` at `09f70832` (2026-11-11). Per-lane evidence: `artifacts/notes/lane-<name>-<date>.md`.
Items marked (?) were not re-verified since the 2026-10 reconciliation.

## Known bugs

### Kernel / VFS
- **Global scans on last close** of tmpfs/mounted files walk every fd table (O(all fds)); a per-object refcount would retire it and `FD_COPY_GATE`.
- **Wake scans are O(parked pollers)**; keyed wake still dereferences each mask match (~11 us per scan). A per-tag index / kbits bloom would fix it.
- **The switch-back CPU charge still takes RUN_QUEUE** (~22.5k holds/s); needs a lock-free `on_cpu` release protocol.
- **New writers of signal state must call `sig_kick`**; a miss shows as `miss` > 0 in `/proc/rqhold` (invariant, no known violation).
- **Flakes to root-cause**: `epolltest unrelated_unix_traffic_no_wake` (11% vs 1/8 threshold in a busy session), `sigtest` (?), `mnttest` tfs_* ordering.
- **F2FS fsync log is private-format, not Linux's**: fsync of a regular file is already durable without a checkpoint (roll-forward records in the checkpoint pack, lanes f2fsync/f2fsrf; fsync p50 4.7 ms -> 0.3 ms) and the mount rolls Linux-written fsync chains forward, but Linux itself cannot recover a file fsynced by this server after a crash (it ignores the LRF1 records). Needs a warm/hot node-log write side with FSYNC/DENT marks (curseg-sequential node allocation + SSA/summary updates). See artifacts/notes/lane-f2fsfsync-2026-11-11.md.
- **Interrupt-context serial diagnostics can drop under back-pressure** (by design).
- **fd-limit fence loss**: EMFILE in a virgl client's submit kills its context (as Linux); fd headroom is the defence.
- **Host-refused `RING_IDX` submit is never completed by QEMU** (descriptors lost); Mesa cannot reach it. Do not rewrite RING_IDX kernel-side.
- **Pty background-child SIGHUP on su session end** unverified against Linux (?).

### Podman / namespaces / cgroups
- **Unconverted `host_root()` privilege checks** (SYS_TIME/SYS_BOOT/SYS_MODULE/RAWIO/WAKE_ALARM done in lane/capclock; rest still all-or-nothing): vt.rs (CAP_SYS_TTY_CONFIG), net raw sockets (CAP_NET_RAW), evdev (CAP_SYS_ADMIN, verify), block.rs (CAP_SYS_ADMIN/CAP_SYS_RAWIO), vfs tmpfs chown/chmod (CAP_CHOWN/CAP_FOWNER), ACCT/VHANGUP/SWAPON (CAP_SYS_PACCT/TTY_CONFIG/SYS_ADMIN), ptrace/kill-like (CAP_SYS_PTRACE/CAP_KILL), mount (CAP_SYS_ADMIN), xattr (CAP_SYS_ADMIN/FOWNER), f2fs (CAP_DAC_OVERRIDE); sys_secprobe/dbg_serial/spawn stay kernel-private.
- **net/ipc/uts namespaces share one slot-kind class** (LOW; fold into per-kind).
- **mntunshare is not an isolation boundary for root-run workloads** (M6).

### Network
- **No IPv6 / AF_INET6** (sshd `[::]:22` fails with EAFNOSUPPORT; podman IPv6 deferred).
- **UDP/IP gaps**: no PMTUD, no IGMPv3/SSM, no IP fragmentation/reassembly (?).
- ~~**Real NIC drivers program no multicast filter**~~ FIXED 2026-11-11 (lane/nicmcast): macb, e1000e, r8169, iwlwifi already accepted all multicast; brcmf (Pi Wi-Fi) now sets `allmulti` (Pi wifi0 rx of 30 mDNS-group datagrams: +5 before, +37 after). Pass-all, no per-group list. See artifacts/notes/lane-nicmcast-2026-11-11.md.
- **iwlwifi gaps** (lane iwlgaps 2026-11-11: WMM later-beacon updates, STATISTICS_NOTIFICATION and antenna comparison were already implemented; sparse ping loss 0/300 on LeandrOS vs 4/300 on Linux, same AP; 2.4 GHz is channel noise): still open: TX MIMO/STBC/SGI on 5 GHz unmeasured (the saved 5 GHz profile/passphrase is not on the stick; add the profile and A/B `iwlwifi.rscol`/`txmimo`).
- **Laptop SSH came up late on two 2026-10 boots** (not reproduced in ten later boots; watch).
- **`netwatch.loss=S` not exercised in QEMU.**
- **Podman published ports**: rootful `-p` and `--internal` firewall semantics open (iptables-shim design: artifacts/notes/lane-podports-2026-11-11.md); TCP NAT port reservation done; host-NIC reach deferred.
- **r8169 RX stalls/chip resets on the PC**: ISR-mask fix needs PC confirmation (R8169 as a loadable module also deferred).

### Desktop / GPU / COSMIC
- **Venus/Vulkan**: cross-open dmabuf import deferred (`artifacts/notes/m9-crossopen-dmabuf/`), aarch64 vkwl untested; Venus submit still synchronous (parks on fence IRQ, no async completion).
- **i915** (cursors to 256x256, async flips, hardware WAIT_VBLANK and IN_FENCE_FD landed in lane/i915feat): `WAIT_VBLANK` polls PIPE_FRMCOUNT every 500 us, not IRQ-driven; `DRM_VBLANK_EVENT`/`SIGNAL` return EINVAL; `IN_FENCE_FD` is a CPU wait before the commit, so a NONBLOCK commit blocks on an unsignalled in-fence; wide cursors use the capped plane-1A watermark (no own skl_compute_wm); async flips need the on-screen fb's pitch/size.
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
