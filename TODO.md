# LeandrOS — TODO

Remaining work only. Finished items are deleted, not marked done; `git log` is the record.
Reconciled against `main` at `09f70832` (2026-11-11). Per-lane evidence: `artifacts/notes/lane-<name>-<date>.md`.
Items marked (?) were not re-verified since the 2026-10 reconciliation.

## Known bugs

### Kernel / VFS
- **ns-0 mount ref race**: a mount detached in the window between open() resolving the path and the open file taking its mount reference is not recorded (lane mntref); that fd falls back to path lookup for flags.
- **New writers of signal state must call `sig_kick`**; a miss shows as `miss` > 0 in `/proc/rqhold` (invariant, no known violation).
- **Flakes to root-cause**: `syscalltest` pidfd_wake_conmon (twice: aarch64 box 2026-11-14 lane lapwifi, x86_64 box 2026-11-15 lane pamrs; passed alone on rerun both times); `sigtest` timed_wait_stop_resumes_remainder (not captured; waitpid(WUNTRACED) 1 s lateness fixed 2026-11-11, re-watch).
- **F2FS fsync log is private-format, not Linux's**: fsync of a regular file is already durable without a checkpoint (roll-forward records in the checkpoint pack, lanes f2fsync/f2fsrf; fsync p50 4.7 ms -> 0.3 ms) and the mount rolls Linux-written fsync chains forward, but Linux itself cannot recover a file fsynced by this server after a crash (it ignores the LRF1 records). Needs a warm/hot node-log write side with FSYNC/DENT marks (curseg-sequential node allocation + SSA/summary updates). See artifacts/notes/lane-f2fsfsync-2026-11-11.md.

### Podman / namespaces / cgroups
- **Namespace table size**: all kinds share one 256-slot table, so the per-kind limits (`/proc/sys/user/max_*_namespaces`, defaults user 72, mnt 48, net 32, pid 20, cgroup/ipc/uts 24, time 8; sum capped at 255) are far below Linux. Grow the shared table so per-kind limits can be Linux-like (Linux defaults ~ /proc/sys/user/max_*_namespaces = 63k).

### Network
- **IPv6 beyond loopback** (L1 ::1, L2 dual-stack/V6ONLY/v4-mapped, L7 resolver (musl) and L4 NIC link-local fe80::/64 (EUI-64, disable_ipv6) + sin6_scope_id + /proc/net/if_inet6 + NDP to the slirp router, L3 ICMPv6 ping/raw sockets + ICMP6_FILTER + hop limit/tclass/pktinfo options and cmsgs + `ping -6` done on ::1; sshd listens on `[::]:22`. Known gaps: v4-mapped destination on a v6 ping socket is EINVAL, hop limit 0 reads back as the default, no ICMPv6 cmsgs. SLAAC/RA (L5: RS x3, RA -> global addr + on-link prefix + default router + lifetimes, /proc/net/ipv6_route) done; no DAD, no DHCPv6/RDNSS, RA MTU ignored; rtnetlink v6 (L6: GETADDR/GETROUTE, NEWADDR/DELADDR/NEWROUTE/DELROUTE, static v6 routes in best(), /proc/net tcp6/udp6/raw6) done, open: RTNLGRP v6 notifications, IFLA_AF_SPEC AF_INET6, busybox `ip -6` output unchecked; own-address local delivery of fe80 on the NIC stack, scope id in recvfrom/accept sockaddrs yet: design-ipv6 L4+). L8 (v6 on veth/bridge/tap in any netns) done; bridge6 (netns<->netns and netns<->bridge address ping6/TCP) and v6 default/gateway routes inside a netns done (ip6br); L9 phase 1 done (ip6pod: rootful podman bridge NAT66 outbound, TCP/UDP/ICMPv6 echo, v6 conntrack + NDP cache, nft ip6 masquerade; box podbridge v6 ping/dns/tcp green); L9 phase 2 DNAT66 done (-p publish over v6: [::1], NIC address, container-to-host; podbridge publish_lo/_nicip/_ctr green). Still open on L9: ICMPv6 errors for forwarded flows (podman_v6_time_exceeded is KNOWN-OPEN in podbridgetest: Time Exceeded is not delivered, unknown whether fwd6::divert reaches time_exceeded at fwd6.rs:139 or the container traceroute6 path is at fault; port-unreachable translation untested), rootless pasta v6 (podpastatest run 2026-11-15 on aarch64 in lane nslimits: pasta_v6_addr_route and pasta_v6_ping FAIL, ping6 "permission denied"; v4 rootless cases pass; no main baseline yet), aardvark-dns fails at --rm teardown on DNS-enabled custom networks (podbridgetest uses --disable-dns); tun6 IFF_TUN 0x86DD + NS proxy, DAD.
- **UDP/IP gaps**: PMTUD works (per-netns cache, TCP clamp, IPv6; udptest `udp_pmtud*`/`tcp_pmtud_clamp`/`udp6_pmtud`). Open: no wire-level check that WANT-mode datagrams leave with DF clear / fragment at the cached PMTU; `IP_RECVERR` does not queue `EMSGSIZE`; no IGMPv3/SSM (`IP_ADD_SOURCE_MEMBERSHIP`, `IP_BLOCK_SOURCE`, `MCAST_JOIN_SOURCE_GROUP`, rx source filter, v3 reports; design: per-socket source filter in `mcast.rs` applied to the smoltcp udp socket). Closing a raw ICMP socket right after `sendto` drops the still-queued packet (Linux sends synchronously). (IP fragmentation/reassembly works: udptest `udp_huge` + peer_frag_echo guard it.)
- **Loopback answers only 127.0.0.1 and 127.0.0.2**: Linux owns all of 127/8; TCP to 127.0.0.3 gets ETIMEDOUT and UDP to 127.0.0.4/.5 no route in this netstack.
- **iwlwifi gaps** (lane iwlgaps 2026-11-11: WMM later-beacon updates, STATISTICS_NOTIFICATION and antenna comparison were already implemented; sparse ping loss 0/300 on LeandrOS vs 4/300 on Linux, same AP; 2.4 GHz is channel noise): still open: TX MIMO/STBC/SGI on 5 GHz unmeasured (the saved 5 GHz profile/passphrase is not on the stick; add the profile and A/B `iwlwifi.rscol`/`txmimo`).
- **Laptop SSH came up late on two 2026-10 boots** (not reproduced in ten later boots; watch).
- **Podman published ports**: rootful bridge `-p` works through netavark `firewall_driver = "nftables"` (lane podnft, podbridgetest); open: filter chains only on new flows, no routed forwarding without NAT, rootful `--network=pasta` is rejected by podman itself, `--internal` is the forwarding=0 switch only.
- **Podman published ports, NIC learn-miss parking**: NIC-side learn-miss parking needs hostfwd (L3); the same gap drops the first SYN of a cross-bridge DNAT connection (ARP learn), costing a ~1 s retransmit. (First-connect latency FIXED lane synlat: the hairpin ARP request was queued without a kick and waited ~450 ms for the next net tick; now 0 ms, brtest gates < 100 ms.)

### Desktop / GPU / COSMIC
- **Venus/Vulkan**: cross-open dmabuf import LANDED (51d92486, venustest phase 5b 11/11 on x86_64 KVM + aarch64 TCG, box 2026-11-13), aarch64 vkwl measured 2026-08-07 (`artifacts/notes/m9-vkwl-aarch64/`); Mac QEMU has no virtio-gpu-gl so Venus is box-only; venustest `virtgpu_wait_nowait_busy_after_kick` + `phase7_fence_fd_unsignalled_at_return` are SKIP (not fail) on aarch64 TCG: NOWAIT never sees a pending fence even behind a 64-submission burst, i.e. fence retires before the ioctl chain returns (measured box 2026-11-13, lane/venuswait; an in-fence cannot make it pending, it only blocks the kick); x86 KVM still asserts them; Venus submit still synchronous (parks on fence IRQ, no async completion).
- **i915** (cursors to 256x256, async flips, hardware WAIT_VBLANK and IN_FENCE_FD landed in lane/i915feat): `WAIT_VBLANK` polls PIPE_FRMCOUNT every 500 us, not IRQ-driven; `DRM_VBLANK_EVENT`/`SIGNAL` return EINVAL; `IN_FENCE_FD` is a CPU wait before the commit, so a NONBLOCK commit blocks on an unsignalled in-fence; wide cursors use the capped plane-1A watermark (no own skl_compute_wm); async flips need the on-screen fb's pitch/size.
- **Laptop**: Wi-Fi link still weak on 2.4 GHz. (Firefox canvas-2D CPU raster, open: same Alpine Firefox binary, LeandrOS ~= Linux arm64 (10.5-11.2 vs 12.6-12.9 fps, artifacts/notes/lane-canvasperf-2026-11-14.md), so not an OS gap; the Alpine Firefox build is ~3x slower than the distro Firefox (hot spot: Skia's scalar edge walker in libxul), next step is comparing build flags; x86_64 KVM guest fps not measured, the harness doesn't capture stdout, so write CANVASPERF to a file or use ffsession.py.) GPU-bound paths are fine (WebGL 60 FPS, 1080p heavy shader 35 FPS at 100% render-engine busy).
- **Minimize applet un-hide unverified** (needs a live session with an open window; applet is a 1x1 hidden surface with zero toplevels by upstream design). `cosmic-files-applet` smithay-clipboard OOM panic: no root cause, needs an in-guest repro with RUST_BACKTRACE (triage 2026-11-13: not reproduced, no runtime log kept). `cosmic-notifications` panel-dbus Broken pipe: the p2p zbus socketpair made in `NotificationsSocket::get_fd` (src/subscriptions/applet.rs) breaks when the applet end closes (applet restart / panel respawn); likely benign upstream behaviour, unverified. Greeter's cosmic-comp logs `eglMakeCurrent BadDisplay` on exit (cosmetic, teardown order).
- **Panel-embedded applets render with tiny-skia** (root cause: cosmic-panel creates zwp_linux_dmabuf_v1 only if its GLES renderer exists at bind_display(); ours comes ~0.3 s later; measured 2026-11-13 aarch64: comp->panel start 0.42 s, no kernel DRM step visible in that window (handoff/klogms.md); needs a cosmic-panel source change, out of bounds - not a kernel bug); aarch64 desktop clicks need a physical-mouse check (P0 #5).
- **VT switching**: a present inside its handler during a switch can flash; evdev VT gate not tested against cosmic-comp. (`EVIOCGRAB` exclusivity, `EVIOCREVOKE` and the Ctrl+Alt+Esc escape hatch are implemented and covered by `vttest`; see artifacts/notes/lane-evdevvt-2026-11-13.md.)

### Hardware (laptop / PC / Pi)
- **Stick write commands of 0.5-1.3 s are hidden, not removed** (USB hubs now supported, usbhot2).

## Missing features

### Kernel
- **nitrox86 changed PIT detection** (port 0x61 bit5 must read low right after programming ch2): verify on the real laptop/PC.
- fanotify: permission events (`FAN_CLASS_CONTENT`/`PRE_CONTENT`, `FAN_OPEN_PERM`/`FAN_ACCESS_PERM` with a verdict `write`), `FAN_MARK_IGNORED_MASK`, `FAN_REPORT_FID`/`DFID_NAME` (`FAN_CREATE`/`DELETE`/`ATTRIB`/`MOVED_*`) and a real mount-root lookup for `FAN_MARK_MOUNT` (today it covers the marked path's subtree) are not implemented; they return `EINVAL`.
- **Interrupt paths**: virtio-input, virtio-net and the virtio-gpu control queue (MSI-X on x86_64, shared INTx SPI on aarch64) are interrupt-driven already. Still polled: virtio-blk (`wait_for_completion` busy-waits the used ring; callers include page faults with IRQs masked, so an IRQ/sleep path needs a masked-context fallback) and virtio-snd TX (reaped by the 100 Hz tick pump, which must run anyway to keep QEMU's stream queue non-empty, so an IRQ gains little).
- **Synthetic sysfs**: `/sys/class/drm`, `/sys/class/input/event{0,1}` and the `/sys/dev/char` node files are staged on disk (static); hot-plugged evdev nodes (event2+) have no `/sys/class/input` entry (the libudev shim learns them from `/dev/input/hotplug`), and `capabilities/*` for event0/1 are fixed approximations. Everything else outside `/sys/class/block` stays disk-staged.
- **utmpx gaps**: no `w`/`users` consumer (Alpine busybox has neither); `/var/log/lastlog` is written but has no reader (busybox `lastlog` absent); su/sudo sessions are not recorded; concurrent writers rely on flock only.
- **VT console**: TAB advance and double-width glyphs.
- **sshd**: privilege separation is not done (sessions are forked children, but the listener and the pre-auth russh code run as root; design in `docs/design/sshd-privsep.md`). Not implemented: `cert-authority` keys, `-D`/streamlocal specific handling, `Match` blocks, `AllowTcpForwarding` in `sshd_config` (forwarding is gated per key only). `environment=` with `PermitUserEnvironment yes` and the `$sha256$` rehash write path of a read-only `/etc` are unverified live (the `yes` case needs a second sshd config).

### Network / Wi-Fi
- **WPA3 on real hardware**: SAE/PMF/roaming exist in the connection manager and are green against `wifisim`, but the Intel (iwlwifi) and brcmf radios do not report `wifi_caps::SAE`/`PMF`/`ROAM` yet (firmware key install for BIP/IGTK, SAE auth frames), and the SAE exchange is unverified against a real WPA3 AP (no WPA3 network was available to the lane).
- **iwlwifi**: HT40 on 2.4 GHz (`iwlwifi.ht40_24`, coexistence check in `wifi_ht::coex_40_24`) is implemented but unverified on hardware (no 40 MHz 2.4 GHz AP seen; the only reachable AP advertises 20 MHz). 5 GHz is untested (needs the user's 5 GHz SSID/passphrase on the stick). Throughput vs Linux not measured. Rate-control idea: column re-entry hysteresis / stay-in-column timer in `iwl_rs.rs`.

### Desktop / apps
- **xdg-desktop-portal** (screenshot, file chooser), timedate1, SwitcherooControl and the a11y bus D-Bus services.
- **`cosmic-store` backend** (PackageKit service or libflatpak); **`cosmic-player`** (needs GStreamer closure, ~1-2 days).
- **A permanent Vulkan-Wayland test** (needs a dynamic-musl build shape).
- **VRR** (`VRR_ENABLED` DRM property).
- **Mesa v3d / Pi 5 acceleration** wiring (`GET_PARAM`+`CREATE_BO` first; single-device design).

### Firefox
- x86 KVM audio verification (?).
- **Sandbox follow-ups**: secirq, zaplog, credxlate residuals (`lane-secirq/zaplog/credxlate-2026-11-09.md`).

### Cloud
- **Boot on AWS EC2** (Nitro instances, x86_64 and Graviton aarch64): DONE: L1 image (`scripts/ec2-image.sh` + `scripts/ec2-ami.sh`, docs/hardware/aws-ec2.md), L2 Amazon EBS NVMe quirks (`nvmeebs` suite; real EBS verification still open: QEMU cannot set the EBS model string), L4 ENA driver core (mock-tested, `drivers/src/ena.rs`/`ena_hw.rs`, `drivers/ena-tests`; unproven on real EC2), L6 ACPI-only aarch64 boot in QEMU (`acpi=force`, a64acpitest 7/7), L3 Nitro x86 timers (no PIT: PIT-absent detection, kvmclock TSC, APIC timer calibration; `nitrox86` suite), L7 ec2init (IMDSv2 SSH key/hostname/user-data on first boot). L5 ENA LLQ (device placement, BAR2 write-combining, `ena.llq=0`; mock-verified only, layouts from memory of the public headers), Still open: L8 MSI-X / L9 GIC ITS, multi-segment PCI/MCFG (only the first ECAM segment is used), 16550-MMIO SPCR untested, the kvmclock TSC path never run on real KVM without CPUID frequency, and real-EC2 proof of the ENA/LLQ layouts, the gated real-EC2 G1/G2 runs (need the user's OK and AWS credentials). Design: orch/designs/design-ec2.md (lane ec2).

### Installer
- **Dual-boot**: no whole-disk wipes; Windows/NTFS and new-machine flows per `docs/design/dualboot-methodology.md`.

### Toolchain / dev experience
- **zig to clang** migration (est. 2-4 days, not started).
- **Claude Code in-guest**: `claude-run` only appends to `/etc/passwd`/`group`; only 2.1.296 is pinned; x86_64 VT/cosmic-term runs not screenshotted.
- **Test harness**: run with `python3 -u` and no pipe; pass `-D <file>` when host tracing is on.

## Notes (not work items: by design, upstream-blocked, out of scope, or deferred by the user)

- **smoltcp "handle does not refer to a valid socket" panic (watch)**: hardened at the source (lane smolpanic2: vendored SocketSet get/get_mut resolve a stale handle to an inert stand-in, remove() of one is a no-op; brtest `stale_race` guards it); the 2026-11-14 recurrence on lane/ip6dnat was never reproduced. If a panic with this text shows again, the stale access is in a caller that kept a handle across a stack reap.
- **Interrupt-context serial diagnostics can drop under back-pressure** (by design).
- **fd-limit fence loss**: EMFILE in a virgl client's submit kills its context (as Linux); fd headroom is the defence.
- **Host-refused `RING_IDX` submit is never completed by QEMU** (descriptors lost); Mesa cannot reach it. Do not rewrite RING_IDX kernel-side.
- **Laptop SSH came up late on two 2026-10 boots** (not reproduced in ten later boots; watch).
- **r8169 RX stalls/chip resets on the PC**: ISR-mask fix needs PC confirmation (R8169 as a loadable module also deferred).
- **One permanent COSMIC patch** (`ports/cosmic-comp/0001-toplevel-info-weak-window.patch`) until upstream has an equivalent (checked 2026-11-13: pop-os/cosmic-comp#2084 and Smithay#1562 still OPEN, PR #2095 closed unmerged - patch stays).
- **Real-PC (RTX 3070) verification deferred ~1 month** (user, 2026-10-24).
- **XWayland, accountsservice, hotplug, multi-seat** are explicitly out of scope.
- **Box farm: shipped port trees must match the ref (watch)**: box regress ships the Mac worktree trees and box build-all rejects a tree whose `.srchash` differs from the ref port sources (box cannot rebuild container ports: no binfmt). Cause seen 2026-11-14/15: lanes ran `git merge main` (which changed ports/portal, ports/gstreamer) and launched box runs without re-running build-all on the Mac; several then hand-stamped `.srchash`, which ships stale trees. Rule now: build-all after every main merge. Open: make regress*.sh pre-check the hashes on the Mac and fail fast instead of burning a box slot (build fails ~40 s, runtests then reports only "boots failed 1/1").
