# LeandrOS — TODO

Remaining work only. Finished items are deleted, not marked done; `git log` is the record.
Reconciled against `main` at `09f70832` (2026-11-11). Per-lane evidence: `artifacts/notes/lane-<name>-<date>.md`.
Items marked (?) were not re-verified since the 2026-10 reconciliation.

## Known bugs

- [ ] mntiso E7: add a host_to_view hint to the fd_ns_path reopen so links whose target is only reachable from another view resolve without the identity-check fallback (design-mntiso.md E7; open after lane/mntiso-procfd).
### Kernel / VFS
- **New writers of signal state must call `sig_kick`**; a miss shows as `miss` > 0 in `/proc/rqhold` (invariant, no known violation).
- **Flakes to root-cause**: `sigtest` timed_wait_stop_resumes_remainder (not captured; waitpid(WUNTRACED) 1 s lateness fixed 2026-11-11, re-watch).
- **F2FS fsync log is private-format, not Linux's**: fsync of a regular file is already durable without a checkpoint (roll-forward records in the checkpoint pack, lanes f2fsync/f2fsrf; fsync p50 4.7 ms -> 0.3 ms) and the mount rolls Linux-written fsync chains forward, but Linux itself cannot recover a file fsynced by this server after a crash (it ignores the LRF1 records). Needs a warm/hot node-log write side with FSYNC/DENT marks (curseg-sequential node allocation + SSA/summary updates). See artifacts/notes/lane-f2fsfsync-2026-11-11.md.
- **Interrupt-context serial diagnostics can drop under back-pressure** (by design).
- **fd-limit fence loss**: EMFILE in a virgl client's submit kills its context (as Linux); fd headroom is the defence.
- **Host-refused `RING_IDX` submit is never completed by QEMU** (descriptors lost); Mesa cannot reach it. Do not rewrite RING_IDX kernel-side.

### Podman / namespaces / cgroups
- ~~**Unconverted `host_root()` privilege checks**~~ DONE (capaudit 2026-11-11 + capvfs 2026-11-11: tmpfs/f2fs chown/chmod/DAC/xattr now use the task's real caps in ns 0; only mount-root gates keep `Cred.host_root` = full-cap init-ns root; sys_secprobe/dbg_serial/spawn stay kernel-private).
- **net/ipc/uts namespaces share one slot-kind class** (LOW; fold into per-kind).
- ~~**mntunshare is not an isolation boundary for root-run workloads** (M6)~~ DONE (mntiso chroot/busy/procfd/entry/strict/bindpin 2026-11-11; the remaining reopen hint is the E7 line above).

### Network
- **IPv6 beyond loopback** (L1 ::1, L2 dual-stack/V6ONLY/v4-mapped, L7 resolver (musl) and L4 NIC link-local fe80::/64 (EUI-64, disable_ipv6) + sin6_scope_id + /proc/net/if_inet6 + NDP to the slirp router, L3 ICMPv6 ping/raw sockets + ICMP6_FILTER + hop limit/tclass/pktinfo options and cmsgs + `ping -6` done on ::1; sshd listens on `[::]:22`. Known gaps: v4-mapped destination on a v6 ping socket is EINVAL, hop limit 0 reads back as the default, no ICMPv6 cmsgs. No SLAAC/RA (L5), no global v6 on NICs, own-address local delivery of fe80 on the NIC stack, scope id in recvfrom/accept sockaddrs, netns, podman IPv6 yet: design-ipv6 L4+).
- **AF_INET6 TCP in a non-initial netns stalls pasta's relay** (pastafix 2026-11-11): socket(AF_INET6) in a netns is EAFNOSUPPORT again as a guard; root-cause the readiness/wake bug (rootless `podman run -p`, passt on v6 sockets in the container netns), then drop the guard. Also open: brtest flakes (fwd_tcp, fwd_port_reserved, veth_tcp_reap ~2/5 runs) are independent of it.
- **UDP/IP gaps**: no PMTUD (IP_MTU_DISCOVER/IP_MTU, DF on Frag-Needed), no IGMPv3/SSM. (IP fragmentation/reassembly works: udptest `udp_huge` + peer_frag_echo guard it.)
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
- **Minimize applet un-hide unverified** (needs a live session with an open window; applet is a 1x1 hidden surface with zero toplevels by upstream design). `cosmic-files-applet` smithay-clipboard OOM panic: no root cause, needs an in-guest repro with RUST_BACKTRACE (triage 2026-11-13: not reproduced, no runtime log kept). `cosmic-notifications` panel-dbus Broken pipe: the p2p zbus socketpair made in `NotificationsSocket::get_fd` (src/subscriptions/applet.rs) breaks when the applet end closes (applet restart / panel respawn); likely benign upstream behaviour, unverified. Greeter's cosmic-comp logs `eglMakeCurrent BadDisplay` on exit (cosmetic, teardown order).
- **Panel-embedded applets render with tiny-skia** (root cause: cosmic-panel creates zwp_linux_dmabuf_v1 only if its GLES renderer exists at bind_display(); ours comes ~0.3 s later; needs a cosmic-panel source change, out of bounds - not a kernel bug); aarch64 desktop clicks need a physical-mouse check (P0 #5).
- **VT switching**: a present inside its handler during a switch can flash; evdev VT gate not tested against cosmic-comp; `EVIOCGRAB` exclusive delivery unmeasured; `EVIOCREVOKE` ignored; Ctrl+Alt+Esc swallowed.
- **One permanent COSMIC patch** (`ports/cosmic-comp/0001-toplevel-info-weak-window.patch`) until upstream has an equivalent (checked 2026-11-13: pop-os/cosmic-comp#2084 and Smithay#1562 still OPEN, PR #2095 closed unmerged - patch stays).
- **x86_64/TCG COSMIC settle timing is stale** — re-measure (clock fixed 2026-09-16).

### Hardware (laptop / PC / Pi)
- **Stick write commands of 0.5-1.3 s are hidden, not removed** (USB hubs now supported, usbhot2).
- **Why Limine leaves RAM above 4 GiB out of the HHDM is unexplained** (`lane-fbmm`); aarch64 has no HHDM gap walker.
- **Real-PC (RTX 3070) verification deferred ~1 month** (user, 2026-10-24).

### Build / tooling
- **build-all silently reuses stale container-built ports when Docker is not running** (PipeWire shipped an old sink, lane smallfix 2026-11-11); same class as the buildguard guards: should fail loudly.
- **A port build failure only warns** (busd link failure → stale busd → misleading staleness error on the box, lane boxbuild); make per-port build failures fatal.
- **`ports/gstreamer/build.sh` still references the deleted `ports/firefox/build.sh`** (fixed for pipewire in lane smallfix).

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
