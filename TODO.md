# LeandrOS — TODO

Remaining work only. Finished items are deleted, not marked done; `git log` is the record.
Reconciled against `main` at `09f70832` (2026-11-11). Per-lane evidence: `artifacts/notes/lane-<name>-<date>.md`.
Items marked (?) were not re-verified since the 2026-10 reconciliation.

## Known bugs

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
- **IPv6 beyond loopback** (L1 ::1, L2 dual-stack/V6ONLY/v4-mapped, L7 resolver (musl) and L4 NIC link-local fe80::/64 (EUI-64, disable_ipv6) + sin6_scope_id + /proc/net/if_inet6 + NDP to the slirp router, L3 ICMPv6 ping/raw sockets + ICMP6_FILTER + hop limit/tclass/pktinfo options and cmsgs + `ping -6` done on ::1; sshd listens on `[::]:22`. Known gaps: v4-mapped destination on a v6 ping socket is EINVAL, hop limit 0 reads back as the default, no ICMPv6 cmsgs. SLAAC/RA (L5: RS x3, RA -> global addr + on-link prefix + default router + lifetimes, /proc/net/ipv6_route) done; no DAD, no DHCPv6/RDNSS, RA MTU ignored; rtnetlink v6 (L6: GETADDR/GETROUTE, NEWADDR/DELADDR/NEWROUTE/DELROUTE, static v6 routes in best(), /proc/net tcp6/udp6/raw6) done, open: RTNLGRP v6 notifications, IFLA_AF_SPEC AF_INET6, busybox `ip -6` output unchecked; own-address local delivery of fe80 on the NIC stack, scope id in recvfrom/accept sockaddrs yet: design-ipv6 L4+). L8 (v6 on veth/bridge/tap in any netns) done; bridge6 (netns<->netns and netns<->bridge address ping6/TCP) and v6 default/gateway routes inside a netns done (ip6br); L9 phase 1 done (ip6pod: rootful podman bridge NAT66 outbound, TCP/UDP/ICMPv6 echo, v6 conntrack + NDP cache, nft ip6 masquerade; box podbridge v6 ping/dns/tcp green); open L9 phase 2: DNAT66 (-p publish over v6), pasta v6, ICMPv6 errors for forwarded flows, aardvark-dns fails at --rm teardown on DNS-enabled custom networks; tun6 IFF_TUN 0x86DD + NS proxy, DAD.
- **UDP/IP gaps**: no PMTUD (IP_MTU_DISCOVER/IP_MTU, DF on Frag-Needed), no IGMPv3/SSM. (IP fragmentation/reassembly works: udptest `udp_huge` + peer_frag_echo guard it.)
- ~~**Real NIC drivers program no multicast filter**~~ FIXED 2026-11-11 (lane/nicmcast): macb, e1000e, r8169, iwlwifi already accepted all multicast; brcmf (Pi Wi-Fi) now sets `allmulti` (Pi wifi0 rx of 30 mDNS-group datagrams: +5 before, +37 after). Pass-all, no per-group list. See artifacts/notes/lane-nicmcast-2026-11-11.md.
- **iwlwifi gaps** (lane iwlgaps 2026-11-11: WMM later-beacon updates, STATISTICS_NOTIFICATION and antenna comparison were already implemented; sparse ping loss 0/300 on LeandrOS vs 4/300 on Linux, same AP; 2.4 GHz is channel noise): still open: TX MIMO/STBC/SGI on 5 GHz unmeasured (the saved 5 GHz profile/passphrase is not on the stick; add the profile and A/B `iwlwifi.rscol`/`txmimo`).
- **brtest fwd_port_reserved flake** — not reproducible since the dead-netns drain fix 964c04f3 (lane fwdres 2026-11-13: 0/60 isolated + 0/70 full brtest runs, Mac aarch64 HVF); likely the same root cause. Reopen on the next sighting with the full FAIL + child-exit lines (handoff fwdres.md).
- **Laptop SSH came up late on two 2026-10 boots** (not reproduced in ten later boots; watch).
- **Podman published ports**: rootful bridge `-p` works through netavark `firewall_driver = "nftables"` (lane podnft, podbridgetest); open: filter chains only on new flows, no routed forwarding without NAT, rootful `--network=pasta` is rejected by podman itself, `--internal` is the forwarding=0 switch only.
- **Podman published ports, first-connect latency**: after the parked-SYN replay (lane nftnat2) the first same-bridge connect still takes ~700 ms (brtest prints KNOWN-OPEN); phases not yet measured (client ARP for gateway vs hairpin, ARP reply -> replay -> SYN-ACK -> accept); kernel log has ms stamps, add net-server timestamps. Also open: NIC learn-miss parking needs hostfwd (L3).
- **Unmarked cross-network DNAT (capture g_two_nets_b) unverified**: hair() `!= Masq => return false` should DNAT and route un-NATed (src kept); needs a Ct flag + reverse match.
- **r8169 RX stalls/chip resets on the PC**: ISR-mask fix needs PC confirmation (R8169 as a loadable module also deferred).

### Desktop / GPU / COSMIC
- **Venus/Vulkan**: cross-open dmabuf import LANDED (51d92486, venustest phase 5b 11/11 on x86_64 KVM + aarch64 TCG, box 2026-11-13), aarch64 vkwl measured 2026-08-07 (`artifacts/notes/m9-vkwl-aarch64/`); Mac QEMU has no virtio-gpu-gl so Venus is box-only; venustest `virtgpu_wait_nowait_busy_after_kick` + `phase7_fence_fd_unsignalled_at_return` are SKIP (not fail) on aarch64 TCG: NOWAIT never sees a pending fence even behind a 64-submission burst, i.e. fence retires before the ioctl chain returns (measured box 2026-11-13, lane/venuswait; an in-fence cannot make it pending, it only blocks the kick); x86 KVM still asserts them; Venus submit still synchronous (parks on fence IRQ, no async completion).
- **i915** (cursors to 256x256, async flips, hardware WAIT_VBLANK and IN_FENCE_FD landed in lane/i915feat): `WAIT_VBLANK` polls PIPE_FRMCOUNT every 500 us, not IRQ-driven; `DRM_VBLANK_EVENT`/`SIGNAL` return EINVAL; `IN_FENCE_FD` is a CPU wait before the commit, so a NONBLOCK commit blocks on an unsignalled in-fence; wide cursors use the capped plane-1A watermark (no own skl_compute_wm); async flips need the on-screen fb's pitch/size.
- **Laptop**: low FPS under COSMIC; Firefox on iris unproven; Wi-Fi link still weak on 2.4 GHz.
- **Minimize applet un-hide unverified** (needs a live session with an open window; applet is a 1x1 hidden surface with zero toplevels by upstream design). `cosmic-files-applet` smithay-clipboard OOM panic: no root cause, needs an in-guest repro with RUST_BACKTRACE (triage 2026-11-13: not reproduced, no runtime log kept). `cosmic-notifications` panel-dbus Broken pipe: the p2p zbus socketpair made in `NotificationsSocket::get_fd` (src/subscriptions/applet.rs) breaks when the applet end closes (applet restart / panel respawn); likely benign upstream behaviour, unverified. Greeter's cosmic-comp logs `eglMakeCurrent BadDisplay` on exit (cosmetic, teardown order).
- **Panel-embedded applets render with tiny-skia** (root cause: cosmic-panel creates zwp_linux_dmabuf_v1 only if its GLES renderer exists at bind_display(); ours comes ~0.3 s later; measured 2026-11-13 aarch64: comp->panel start 0.42 s, no kernel DRM step visible in that window (handoff/klogms.md); needs a cosmic-panel source change, out of bounds - not a kernel bug); aarch64 desktop clicks need a physical-mouse check (P0 #5).
- **VT switching**: a present inside its handler during a switch can flash; evdev VT gate not tested against cosmic-comp. (`EVIOCGRAB` exclusivity, `EVIOCREVOKE` and the Ctrl+Alt+Esc escape hatch are implemented and covered by `vttest`; see artifacts/notes/lane-evdevvt-2026-11-13.md.)
- **One permanent COSMIC patch** (`ports/cosmic-comp/0001-toplevel-info-weak-window.patch`) until upstream has an equivalent (checked 2026-11-13: pop-os/cosmic-comp#2084 and Smithay#1562 still OPEN, PR #2095 closed unmerged - patch stays).

### Hardware (laptop / PC / Pi)
- **Stick write commands of 0.5-1.3 s are hidden, not removed** (USB hubs now supported, usbhot2).
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
- [x] ~~ns 0 per-mount flags~~ DONE (ns0flags 2026-11-13): ns-0 alias/bind/self-bind mounts carry ro/nodev/noexec, enforced via mnt_flag_check + exec check; remount replaces flags, MS_BIND|MS_RDONLY single call ignored (Linux); ns-0 dirfds follow the live mount's flags and /proc/mounts + mountinfo print ro/nodev/noexec (ns0mounts 2026-11-13)
- [ ] ns-0 mount identity for dirfds is a hash of the mountpoint path (`alias::id_of`, lane ns0mounts 7f8edbcc): after a umount + new mount at the same path, an old dirfd picks up the NEW mount's flags (Linux keeps the detached vfsmount, so a ro bind stays ro). Root-only to trigger; give ns-0 mounts a unique generation id instead.
- /proc/mounts doesn't print ro for ns-0 mounts; ns-0 dirfd flags recorded at open go stale after a later remount
