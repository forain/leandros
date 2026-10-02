# Lane firefox — 2026-09-27

## RESUME HERE (2026-10-02, after merging lane/x64crash and lane/ffmagenta)
Main now has both 2026-10-02 lanes: the x86_64 crash fixes (DF on kernel entry, MADV_DONTNEED, free-after-flush) and the magenta/stale-content fixes (guest Mesa patch 0002, VIRTGPU_WAIT EBUSY, real VIRTGPU_TRANSFER_*). Their sections follow, ffmagenta first.

State: Firefox loads http/https pages on both arches with GPU WebRender (virgl on the Mac, virgl or Venus/zink on the linux desktop), no magenta, no stale content, no crash in any verification session, including x86_64 under KVM.

**Run Firefox sessions with 4G guest RAM** (`ffsession.py` now defaults `LEANDROS_QEMU_MEM=4G`). At the driver's 2G default, init's memory-pressure guard kills Firefox a few seconds into startup on the virgl path (`MEMORY PRESSURE ... killed pid N (firefox), RSS ~350-400 MiB`, ff.log `EXIT=137`, no window ever drawn). Seen on aarch64/HVF and on x86_64/KVM virgl; x86_64/KVM Venus survived at 2G. Not a regression: the merge, pre-merge main `6703408` and the lane tip `90f5b7d` all behave the same at 2G and the old host virglrenderer too; the lanes ran at 4G.

### Integration verification (merge `3b6507e`, 2026-10-02)
Shared state updated (backups are timestamped copies next to the originals, suffix `.bak-20261002-0913`):
- Shared GPU stage, Mac: `~/code/leandros-artifacts/m3-gl-stack/gpu-stage-{aarch64,x86_64}` rebuilt with patches 0001+0002 (aarch64 on the Mac under Docker, x86_64 natively on the linux desktop under podman; both logs show `applying .../0002-virgl-no-triangle-fans-on-gles-hosts.patch` and end `=== rc=0`). Every file is bit-identical to the lane's verified private stage; against the old stage only `libgallium-25.3.6.so` and `gpuprobe` differ. The linux desktop's shared stage holds the same two trees.
- Host QEMU on the Mac: `scripts/mac-qemu-gpu/build.sh --angle-vulkan --force virgl` installed the fan-free virglrenderer into `~/.local/qemu-gpu-gles31` (only `lib/libvirglrenderer*` and `bin/virgl_test_server` changed; QEMU links the library by absolute path, no QEMU rebuild). Every Mac session below ran on it.

Results:
- `./scripts/build-all.sh`: OK on the Mac and on the linux desktop (worktree on the second NVMe, branch `merge/ffmagenta` there).
- 13 suites + vfstest via runtests.py: **14/14 RC=0** on aarch64/HVF, x86_64/TCG and x86_64/KVM (desktop). On the desktop, polltest `poll_linux_semantics` failed until its sibling `relibc` checkout had `fd1967e1` (it was fast-forwarded mid-session); rebuilt, it passes.
- drmsmoke (`RUNTESTS_VIRGL=1`): `failed=0` on aarch64/HVF, x86_64/TCG and x86_64/KVM; TRANSFER_ROUNDTRIP and VIRTGPU_WAIT_NOWAIT_EBUSY pass everywhere.
- Firefox, 4G, magenta by `magcount.py` over every screenshot: 0 everywhere, no EXIT line, no SEGV/Scudo/memory-pressure line:
  - Mac aarch64/HVF virgl: Wikipedia (150 s) and example.com (150 s);
  - Mac x86_64/TCG virgl: Wikipedia (200 s) and example.com (150 s);
  - desktop x86_64/KVM Venus/zink on RADV (2G) and virgl on radeonsi (4G): Wikipedia, alive for the whole 200 s wait plus setup (over 3 min), Wikipedia and Firefox logos present. Proof: `firefox-integ-wikipedia-x86_64-kvm-virgl.png`.
  - This closes the x64crash "TCG vs KVM" check: no crash under KVM either.

Open:
- Upstream ANGLE should emulate triangle fans when the portability subset lacks them.
- `ffsession.py`'s final process listing prints nothing: the guest has no `grep`.

## Magenta and stale content (lane/ffmagenta, 2026-10-02)
Branch `lane/ffmagenta` on `8de8006`. Not merged, not pushed. Two independent bugs, both GPU-path, neither in Firefox. Software rendering was never used.

### Bug 1: magenta = half-cleared targets (triangle fans on ANGLE/MoltenVK)
**Broken layer: the Mac host stack.** ANGLE's Vulkan backend passes `GL_TRIANGLE_FAN` straight to Vulkan (`vk_utils.cpp` `GetPrimitiveTopology`), and MoltenVK (Metal has no fans) draws only the first triangle of a fan. `hostprobe-fan.c` against the installed `~/.local/qemu-gpu-gles31` ANGLE: a full-target 4-vertex quad drawn as a fan covers exactly 32896/65536 pixels (half, one triangle), the same for indexed and instanced fans. A strip and a triangle list are correct. An unwritten texel on this stack reads `#FF00FF` (the macmagenta note's probe).
**Who draws fans:** guest Mesa itself. virgl has no scissored-clear cap, so every scissored or color-masked `glClear` goes through st `clear_with_quad`, which is `st_draw_quad`, a 4-vertex fan. WebRender clears picture-cache and render-task targets with scissored `glClear`. Half of each cleared rectangle kept whatever the texture held, which was magenta. That explains the diagonal edges, the triangles and the pinwheel-shaped radio buttons. Host-side, virglrenderer's shader blitter (`vrend_blitter.c`, used for swizzled/BGR*/converting blits) also draws a fan.
**Evidence chain (aarch64/HVF, Wikipedia):**
- baseline: 24,926 magenta px;
- `gfx.webrender.scissored-cache-clears.enabled=false`: 91 px, with a half-magenta search icon left;
- guest Mesa without fans: 0 px in every one of 12 sessions since.
**Fixes:**
- `ports/mesa/patches/0002-virgl-no-triangle-fans-on-gles-hosts.patch` (the guest-side workaround): on a GLES host (`VIRGL_CAP_HOST_IS_GLES`, where ANGLE runs) virgl clears the fan bit from the host `prim_mask`, so `virgl_draw_vbo` sends fans through `u_primconvert` as indexed triangle lists, as it already does for quads. It stays on the GPU.
- `scripts/mac-qemu-gpu/patches/virglrenderer-1.3.0-no-triangle-fans.patch` (the real-layer fix for our own host build): the blitter draws a strip, and on macOS GLES the caps drop the fan bit. Verified from a private prefix (stock guest Mesa: magenta gone). **Not installed:** the shared `~/.local/qemu-gpu-gles31` is untouched. Install with `scripts/mac-qemu-gpu/build.sh --angle-vulkan --force virgl` when no QEMU from it is running.
- Not fixed upstream: ANGLE should emulate fans when `VkPhysicalDevicePortabilitySubsetFeaturesKHR::triangleFans` is false.

### Bug 2: stale, missing and misplaced content (VIRTGPU_WAIT errno)
With the magenta gone, the regions under it showed the second bug: no Wikipedia logo, no Firefox logo, no search icon (or a black square), "Appearance" cut to "ce", a stray "k", and a grey box over the article text. It hit 5 of 8 sessions; the patched-Mesa runs mesa6, mesa7, age2, pbo1 and pbo2 are in the scratch runs. Buffer age (`gfx.webrender.allow-partial-present-buffer-age=false`) and PBO uploads (`gfx.webrender.pbo-uploads=false`) did not change it.
**Broken layer: kernel DRM.** `virtgpu_handle_wait` answered a NOWAIT probe of a busy BO with `DriverError::Io`, which the DRM server maps to errno 1. Mesa's `virgl_drm_resource_is_busy` counts a BO as busy **only on EBUSY**, so every busy BO read as idle:
- `virgl_resource_transfer_prepare`: a DISCARD_RANGE/DISCARD_WHOLE_RESOURCE map (WebRender's instance/vertex/PBO uploads, buffer orphaning) skipped the realloc or staging copy and wrote in place. The host then read the new bytes for already-submitted draws.
- The resource cache handed busy BOs out again.
- `virgl_fence_wait` returned at once.
**Fix `ef82d58`:** Busy (NOWAIT or the 15 s timeout) is EBUSY for this ioctl, and a signal is EINTR (drmIoctl restarts it). After it: 3 of 3 Wikipedia sessions came out complete and pixel-identical.
**Found on the way, `9bc5ebe`:** `DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST/FROM_HOST` ignored their argument and sent a bare header the host refused, then answered 0. Mesa's `transfer_put`/`transfer_get` (MSAA uploads, readbacks without a staging path) moved nothing. They now send a real fenced TRANSFER_*_3D on the caller's context and fence the BO.
Tests, drmsmoke:
- `TRANSFER_ROUNDTRIP`: write a pattern, to host, zero, from host, compare;
- `VIRTGPU_WAIT_NOWAIT_EBUSY`: 64/65 busy answers seen on aarch64, all EBUSY;
- `drmsmoke --transfer` runs just these two.

### Commits
- `9e1698c` ports/mesa: virgl draws no triangle fans on GLES hosts
- `e6c485e` mac-qemu-gpu: virglrenderer draws no triangle fans on macOS
- `ef82d58` drm: VIRTGPU_WAIT answers EBUSY for a busy BO
- `9bc5ebe` drm: VIRTGPU_TRANSFER_TO_HOST/FROM_HOST transfer what they are asked
- a notes/tools commit: this section, `magcount.py`, `hostprobe-fan.c`, `ffsession.py --prefs`, `RUNTESTS_VIRGL=1`, and the proof screenshots

### MERGE NOTE: the shared GPU stage
`build-all.sh` packs Mesa from the shared `~/code/leandros-artifacts/m3-gl-stack/gpu-stage-<arch>`, which this lane did **not** overwrite. The verified images used `LEANDROS_GPU_STAGE=<scratch>/art/m3-gl-stack` (patches 0001+0002). After merging, rebuild the shared stage, then build-all:
- aarch64 on the Mac: `ports/mesa/build-gpu-stack.sh aarch64`, ~10 min.
- x86_64: natively on the linux desktop, or on the Mac under Docker amd64 emulation (~45 min under load, worked).
Until then main's images still have the fan bug (patch 0002 missing). The kernel fixes need no stage.

### Verification (final tree, 9bc5ebe + tools)
- `LEANDROS_GPU_STAGE=... ./scripts/build-all.sh`: OK.
- 13-suite via runtests.py: **13/13 RC=0 on aarch64/HVF and x86_64/TCG**.
- drmsmoke (full and `--transfer`, virgl boot): `failed=0` on both arches.
- Firefox, virgl hardware WebRender, magenta pixel count by `magcount.py`, every screenshot of every session 0:
  - aarch64: Wikipedia 3/3 complete before the proof run, then Wikipedia and example.com;
  - x86_64: Wikipedia and example.com.
  - Desktop (greeter, panel, cosmic-term) came up in all of them.
  - Neither arch crashed: no EXIT line and no SEGV in any of the four proof sessions (x86_64 ran 300 s).
  - Network flake: the first Wikipedia attempt, with both arches booted at once, got "Server Not Found" (DNS) on both. Run again one at a time, it loaded.
- Proof: `firefox-ffmagenta-{wikipedia,example}-{aarch64,x86_64}.png`. The older `firefox-https-wikipedia-aarch64.png` is the before picture.
- Not done: the linux desktop comparison. 172.16.158.150 was unreachable from the Mac's network today, so a non-Mac host was not tested. Per the macmagenta note, unwritten texels are black/garbage there, so bug 1 would show as dark slivers (or not at all), and bug 2 would show the same way.

### Tools
- `magcount.py IMG.ppm...` counts `#FF00FF` pixels and prints their bbox.
- `ffsession.py ... --prefs "k=v;k=v"` writes a throwaway `defaults/pref/zz-ffsession.js`; it is removed on the next run.
- `RUNTESTS_VIRGL=1 runtests.py ...` boots with `--virgl`.
- `hostprobe-fan.c`: host GL fan check, with the build line in the file.

## x86_64 Firefox crash (lane/x64crash, 2026-10-02)
Branch `lane/x64crash` on `8de8006` (main + networking). Not merged, not pushed.

**Two kernel bugs, both reachable on any accelerator; the slow TCG guest only made them likely.**

1. **RFLAGS.DF was never cleared on kernel entry (`0f116bd`).** The hand-written fault/IRQ stubs (page fault, #UD, #GP, timer, reschedule IPI) had no `cld` (the `extern "x86-interrupt"` handlers do, LLVM emits one), and SYSCALL's FMASK cleared only IF. User code runs with DF=1 inside every backward memmove (musl: `std; rep movsb; cld`), exactly where a page fault on the next page or a timer tick lands. The kernel is built for DF=0: `__uaccess_copy` is `rep movsb`, and LLVM lowers struct copies to `rep movsq` (net/drm servers, smoltcp, BTreeMap, clone). With DF=1 they copy downwards. Evidence: the new `memtest df 2` (page faults taken with DF=1) **panicked the kernel** (alloc.rs:573) before the fix and passes after it (parts 1 and 3, syscalls entered with DF=1 and preempted DF=1 spinners, passed before too: guards, not reproducers). The first session of this lane (both bugs present) died in Mesa's NIR compiler with "Unknown jump instruction" while printing an intrinsic (the instruction type it had switched on was not the one in memory); either bug could explain that one. The DF fix alone did not stop the crashes. Fix: `cld` first in every fault/IRQ stub, FMASK = IF|DF|TF|NT|AC.
2. **madvise(MADV_DONTNEED) was a no-op (`06ee29b`).** Firefox on Alpine allocates through Scudo; its secondary cache releases idle blocks with MADV_DONTNEED, marks them `Time = 0` and later serves calloc() from them without a memset (released == zeroed on Linux). A temporary census counted ~27000 DONTNEED calls / ~690 MiB in one Wikipedia session. With only the DF fix, the parent still died in 4 of 7 sessions: 2x `Scudo ERROR: invalid chunk state when deallocating` (double free → MOZ_CRASH at firefox+0x36ed2), 2x a NULL key in a live hash slot in Places' `History::StartPendingVisitedQueries` (libxul vaddr 0x7091012, the "run 3" crash of the previous section). Fix: `AddressSpace::discard_range` drops resident pages of private non-device VMAs (flush first, then release frames); anon reads zero, private file pages re-read the file, shared mappings keep their data. Why only x86_64 showed it is not proven; most likely timing (on slow TCG far more cache entries age past Scudo's release interval than on aarch64/HVF).
3. Found on the way, same class: munmap and brk-shrink released frames **before** the remote TLB flush (`a03b0b2`); now after.

Ruled out, with evidence:
- **User GS base (ARCH_SET_GS, wasm2c segue):** a temporary check compared the live user GS base (KERNEL_GS_BASE after swapgs) with the published per-CPU value on every ring-3 fault/IRQ entry during a full Wikipedia session: 0 mismatches.
- **TLB shootdown ack timeouts:** temporary logging, 0 timeouts in a crashing session.
- **XSAVE/AVX:** CR4.OSXSAVE is never set, so CPUID reports no OSXSAVE and no user code uses VEX; FXSAVE covers XMM0-15.
- **The ffmagenta GPU fixes** (`ef82d58` VIRTGPU_WAIT EBUSY, `9bc5ebe` transfers) **do not cure it:** cherry-picked onto this branch *without* the madvise fix, 2 of 4 sessions still died (a content process at PC 0, the parent on the Scudo double free). They fix rendering, not this.
- **TCG vs KVM: not tested.** The linux desktop and laptop were unreachable from the Mac all session (ssh timeouts). Both bugs are architectural (DF semantics; madvise semantics), and `memtest` reproduces each deterministically on TCG, so KVM would show the same test failures. `-smp 1` was tried but Firefox never finished loading Wikipedia in 300 s on one TCG vCPU, so it says nothing; the DF and madvise tests are single-threaded and need no SMP.

Tests: memtest `direction_flag_kernel_entry` (x86_64; `memtest df [1|2|3]` runs one part: 1 syscalls entered with DF=1, 2 page faults with DF=1, 3 preempted DF=1 spinners) and `madvise_dontneed` (both arches; failed on x86_64 before the fix: anon, fork, file and ENOMEM checks).
Tools: driver.py now takes `LEANDROS_SMP`, `LEANDROS_X86_CPU`, `LEANDROS_TCG_THREAD` (`04f15c8`). `-cpu qemu64` does not boot this kernel (no FSGSBASE, CR4.FSGSBASE is set unconditionally).

Verification on the final tree: `./scripts/build-all.sh` OK; 13-suite **13/13 RC=0 on aarch64/HVF and x86_64/TCG** (229 / 230 PASS lines, 0 FAIL); Firefox https://en.wikipedia.org/wiki/Firefox: **x86_64/TCG 3 of 3 sessions alive and clean for the whole 210 s wait** (page rendered, no SEGV/Scudo/channel error; plus 3 of 3 earlier with the madvise fix alone), **aarch64/HVF 1 of 1 clean** (180 s). Tally before the madvise fix, DF fix in: 4 of 7 sessions crashed; with the ffmagenta GPU fixes instead of it: 2 of 4.

Commits: `0f116bd` DF, `6eecbe9` its test, `06ee29b` madvise, `9199fcb` its test, `a03b0b2` free-after-flush, `04f15c8` driver knobs, then this note.

Open: TCG-vs-KVM still unrun (box unreachable); magenta/stale-content rendering is lane/ffmagenta's.

## Previous RESUME HERE (networking, 2026-10-01, superseded)
Branch `lane/ffnet`, worktree `.claude/worktrees/agent-a40da30690bad16e4`, on top of `c9e0ba3` (main with the Firefox lane merged). Not merged, not pushed.

**State: Firefox loads http and https pages on both arches.** Plain HTTP from a host server and by name (neverssl.com), and HTTPS (example.com over HTTP/3/QUIC, en.wikipedia.org over TLS/TCP, HTTP/2) are screenshot-verified on aarch64/HVF and x86_64/TCG. The lock icon shows, so NSS validated the chain with its builtin roots (`libnssckbi.so` is staged, Mozilla Builtin Roots) against the guest wall clock. Proof images in `artifacts/notes/lane-firefox-tools/`:
- `firefox-http-host-{aarch64,x86_64}.png`: http://192.168.105.1:8080/ (python http.server on the Mac)
- `firefox-http-neverssl-aarch64.png`: http://neverssl.com/ (DNS + plain HTTP)
- `firefox-https-wikipedia-{aarch64,x86_64}.png`: https://en.wikipedia.org/wiki/Firefox
- `firefox-https-example-{aarch64,x86_64}.png`: https://example.com/. Its text looks garbled in a still frame: that is the page itself, which fades per-character between six languages (Arabic and Chinese have no glyphs here), not a rendering fault.

Commits:
- `9642b20` run-qemu, driver: give each QEMU its own NIC MAC
- `df498bd` net: unconnected UDP sockets, and datagram sendmsg/recvmsg
- `01eb4ee` net: TCP end of stream on the peer's FIN, and a FIN on close
- `eb8b05d` net: 64 KiB TCP receive window
- `ae73809` net: TCP connect() waits or answers EINPROGRESS; refusals are reported
- `c99e46b` vfs: /etc/resolv.conf names the DHCP lease's DNS servers
- a notes/tools commit: this section, `nettool.c`, the proof screenshots

### Network setup (as found)
- QEMU: `-netdev socket,fd=3` through socket_vmnet when its daemon runs (this Mac: yes), else SLIRP. Guest NIC is virtio-net; the net server runs smoltcp 0.11 with a DHCPv4 client on the NIC stack and a separate loopback stack (127/8). IPv4 only: `socket(AF_INET6, …)` is EAFNOSUPPORT, and Firefox/musl fall back to IPv4 without trouble.
- vmnet gives 192.168.105.x by DHCP; gateway and DNS are 192.168.105.1 (the Mac). `/etc/hosts`, `/etc/nsswitch.conf` (`hosts: files dns`) and `/etc/services` are static VFS RAM entries.
- Firefox uses musl's getaddrinfo (native resolver, TRR off), so DNS is plain UDP to resolv.conf's servers.

### Root causes, with evidence
1. **Every connection to a LAN host died after the handshake: other guests' RSTs (`9642b20`).** `nettool get` connected, then read EOF with 0 bytes, to the Mac and to the linux desktop, while 1.1.1.1 worked. The host server never saw a request. A QEMU `filter-dump` pcap showed each received SYN-ACK answered by two RSTs from "our" IP before our own ACK. Every QEMU NIC has the default MAC 52:54:00:12:34:56; socket_vmnet puts all VMs on one bridge, so the other LeandrOS QEMUs (other worktrees) held the same lease, received our segments and RSTed them (no matching socket). Cloudflare ignored the RST, the Mac and Linux honoured it. Fix: MAC derived from (tree, arch, run id); `LEANDROS_MAC` overrides. Not a kernel bug, but it masks everything else, so check `ps` for other vmnet QEMUs first when the network misbehaves.
2. **No name ever resolved: unconnected UDP did not exist (`df498bd`).** getaddrinfo returned EAI_AGAIN after 5 s. Only connect() created the smoltcp UDP socket; sendto() on a fresh or bound socket hit the `_ => EPIPE` arm and recvfrom() EBADF. musl's resolver is exactly bind(0) + sendto + poll + recvfrom. Same commit: UDP sendmsg/recvmsg sent each iovec as a separate datagram and ignored msg_name (Firefox's QUIC uses them; HTTP/3 to example.com now works), and recv_slice lost datagrams longer than the buffer. Tests: scmtest `udp_unconnected`, `udp_msghdr`.
3. **A response that ended with the server closing never reached EOF (`01eb4ee`).** recv() returned EAGAIN forever in CloseWait (EOF was `!is_active()`, true only once Closed) and poll gave no POLLIN: `nettool get` against a `Connection: close` server timed out after the body. The other half: close() removed the smoltcp socket without sending anything, so peers never saw a FIN (pcap). Close now sends FIN (RST if unread data) and keeps the socket as a reaped orphan. Test: scmtest `tcp_peer_close_eof`.
4. **connect() returned 0 before the handshake, and nothing reported a refusal (`ae73809`).** Non-blocking now answers EINPROGRESS, blocking waits; a refused connect gives POLLERR + SO_ERROR=ECONNREFUSED (NSPR's PR_ConnectContinue reads it) and send() on a dead socket fails instead of EAGAIN forever. Test: scmtest `tcp_connect_refused` (the first version of the test connected a socket to itself: a closed probe's port was handed out again as the ephemeral source port in the same tick; the probe now stays bound).
5. Not bugs, improvements: TCP window 8 KiB → 64 KiB (`eb8b05d`, by arithmetic, not measured), and resolv.conf from DHCP (`c99e46b`; the first query to 8.8.8.8 through vmnet NAT was lost about once per boot, costing musl's 2.5 s retry).

What was checked and was fine: wall clock (guest clock matched the host, certificates validated), getrandom (ChaCha20 since `b97e2526`), NSS builtin roots (staged), getsockname/getpeername, nonblocking poll for POLLOUT, Firefox's socket options (setsockopt accepts everything).

### Verification (final tree)
- `./scripts/build-all.sh`: OK.
- 13-suite via runtests.py: **13/13 RC=0 on aarch64/HVF and on x86_64/TCG**, including the four new scmtest cases.
- Desktop boots (greeter, panel, cosmic-term, Firefox) on both arches are the ffsession runs above.
- `nettool resolve www.wikipedia.org` ~20-70 ms, `nettool get example.com 80 /` both arches.

### Next blocker (x86_64 only, not network)
On x86_64/TCG, Firefox crashes a few seconds after Wikipedia renders (3 of 3 runs; aarch64/HVF did not crash in 3 runs). The crash differs per run:
- run 1: a content process jumps to PC 0 (`[PF] SEGV … pc=0x0`);
- run 2: the parent dies with 139, no page-fault line (so not a page fault: a #GP/#UD delivered silently to Firefox's handler, which re-raises);
- run 3: the parent reads NULL in libxul at vaddr 0x7091012: a loop over an array of `nsIURI*` calling `GetSpec` (vtable slot 3 with an nsAutoCString) hits a null element; the function references "places-shutdown" (Places/history code).
Three different crashes on one arch smell like memory or register corruption rather than a Firefox bug. Candidates, unproven: x86_64 SMP/TLB or context-switch state (note: CR4.FSGSBASE is set, user GS base handling is new in `0c5c9ea`), or TCG itself. Per the machines note, the accelerator is not the arch: try x86_64 under KVM on the linux desktop before blaming the kernel. Locating method: the temporary EL0/user-fault logging described below (print RSP and the words above it plus their VMAs for a null PC; log non-PF user exceptions before `fault_signal`), then file offset = VMA file_off + (pc − VMA start), vaddr = offset + 0x1000 for libxul text on x86_64.

### Tools added
- `nettool.c` (static musl, build line in the file): `resolve NAME`, `get HOST PORT PATH` (non-blocking connect, poll, SO_ERROR, getsockname/getpeername, HTTP/1.0 GET to EOF), `v6`. Stage with `LEANDROS_EXTRA_BIN=<dir>`; `scripts/mkfs-f2fs-populated.py` alone rebuilds an image in about 2 s.
- Packet capture without root: `LEANDROS_QEMU_EXTRA="-object filter-dump,id=fd0,netdev=net0,file=/path/net.pcap"` on `driver.py start`, then `tcpdump -r`.
- Host servers: the Mac's firewall lets python http.server through on 192.168.105.1; the linux desktop (172.16.158.150) is reachable from the guest too.

## Previous RESUME HERE (paused 2026-10-01, superseded)
Branch `lane/firefox`, worktree `.claude/worktrees/agent-a40da30690bad16e4`. Not merged, not pushed. Base `7b28aa05`; everything up to `bb26903` is described further down. The tree is clean.

**State: Firefox 136 renders pages on both arches, on the GPU (virgl, hardware WebRender).** `about:license` and a local `file://` test page are screenshot-verified on aarch64/HVF and x86_64/TCG. Proof images are in `artifacts/notes/lane-firefox-tools/`:
- `firefox-file-page-aarch64.png`, `firefox-about-license-aarch64.png`
- `firefox-file-page-x86_64.png`, `firefox-about-license-x86_64.png`

Commits of this session, after `bb26903`:
- `1bd2177` fd: dup2 a socket onto a VFS-range descriptor. **This was the exit-127 root cause.**
- `fabdb1c` net: pass connected AF_UNIX ends over SCM_RIGHTS
- `9437f52` vfs: open("/proc/self/fd/N") reopens a tmpfs file or memfd
- `cb5a466` mm: an mmap address hint the kernel cannot use is ignored, not EINVAL
- `0c5c9ea` x86_64: per-thread user GS base (arch_prctl ARCH_SET_GS/ARCH_GET_GS)
- `04fd240` ports/firefox: write-protect JIT code in content processes too
- `2ca54ee` net: a short sendmsg write ends the call instead of skipping to the next iovec
- a notes/tools commit: this section, `ffsession.py --url`, and the proof screenshots

### The exit 127, root cause with evidence
None of the earlier hypotheses held: not the fork server, not posix_spawn/CLONE_VFORK, and not an execve ENOENT.
- Firefox's `LaunchApp` (`ipc/chromium/src/base/process_util_linux.cc`) forks, then dup2's each remapped fd onto its target. If a dup2 fails it calls `_exit(127)` before it ever reaches execve. That is why the old execve logging saw nothing.
- The IPC channel end is a socketpair fd (≥ `SOCK_FD_BASE` = 0x100). Its target is a small number (5 or 6).
- `sys_dup3` sent every dup2 to the VFS, which rejects any fd ≥ `MAX_FDS` with EBADF.
- Evidence: temporary logging, since removed, showed every child doing `dup3 old=0x107/0x108 new=0x5/0x6 → -9 (EBADF)` and then `exit_group(127)`. It was 13 out of 13 children on aarch64 and the same on x86_64.

The fix is `VnodeKind::SockAlias` plus a hidden net slot; the commit message has the design. Regression tests: scmtest `socket_dup2_low_fd` and `fork_dup2_low_exec`.

### What was behind the 127 (each found by temporary logging, all fixed above)
1. **The parent's sendmsg failed with EBADF.** Firefox passes connected socket ends over SCM_RIGHTS, and the net server refused them (`fabdb1c`).
2. **Content processes crashed with SIGSEGV at startup in MOZ_CRASH(OOM).** The locations were found with a temporary EL0-fault VMA locator: libxul text, a MOZ_CRASH stub at line 689 whose reason string is "MOZ_CRASH(OOM)", called from JS runtime init.
   - Cause: with `content_process_write_protect_code` off, the content JIT commits its code RWX, and the kernel's mmap W^X check returns EINVAL.
   - Fixed by the pref (`04fd240`). The kernel policy is unchanged.
3. **mmap hints above 128 TiB returned EINVAL.** SpiderMonkey and mozjemalloc make thousands of these random-address probes per process (`cb5a466`).
4. **x86_64 only: the parent aborted on `wasm_rt_syscall_set_segue_base error: Invalid argument`.** This is RLBox wasm2c "segue", which needs ARCH_SET_GS (`0c5c9ea`).
5. **"read-only dup failed; not using memfd".** The `/proc/self/fd/N` reopen was ENOENT (`9437f52`).
6. **Intermittent failures on both arches, about half the runs: `IPDL protocol error: File handle not found in message!`, then an abort and EXIT=139.**
   - The fds were not being lost; the message bytes were corrupt. The no-fd sendmsg path kept writing later iovecs after a short write.
   - The AF_UNIX ring is only 4 KiB, so most Firefox messages are short-written.
   - scmtest `sendmsg_short_write_keeps_stream` failed 5 of 5 before the fix and passes after it (`2ca54ee`).

### Verification (final tree)
- `./scripts/build-all.sh`: OK.
- The 13-suite regression via runtests.py: **13/13 RC=0 on aarch64/HVF and on x86_64/TCG**, including all the new checks:
  - scmtest: socket_dup2_low_fd, fork_dup2_low_exec, pass_connected_socket, memfd_reopen_readonly, sendmsg_short_write_keeps_stream;
  - memtest: mmap_hint_is_only_a_hint;
  - pthreadtest: arch_gs_base (x86_64).
- b97e252 (CSPRNG), the owed verification: 13/13 on both arches before any of the above, and a desktop boot on both arches (greeter, panel, cosmic-term).
- Firefox stability on the final tree:
  - 5 of 5 sessions stayed up for the whole wait with no IPDL error, crash or channel error: aarch64 3 runs (about:license, file://, about:license), x86_64 2 runs (about:license, file://).
  - Before `2ca54ee`, about half the runs on each arch hit the IPDL abort.
  - The guest profile persists on the image, so later runs restore the previous tab next to the new one.

### Open / next
- **Magenta artifacts (both arches, not investigated).**
  - aarch64: a solid magenta bar in the URL bar, right of the text.
  - Both arches: magenta slivers around the back/forward buttons and at the notification-bar edges.
  - Page content is clean. This looks like a WebRender texture or picture-cache problem on virgl/ANGLE rather than a kernel one. Cheap first step: `gfx.webrender.debug.*`, or disable the picture cache to see which cache tiles go magenta.
- x86_64 logs `Failed to create EGLContext!: 0x3009` once. It is benign, because WebRender retries and succeeds.
- `Unable to determine pipe buffer size: Protocol not available`: the kernel has no F_GETPIPE_SZ on sockets. Benign.
- Networking (http/https) is untested. Do this next.
- Kernel debt this lane leaves:
  - A connected AF_UNIX end queued over its own connection keeps the connection alive. It is a leak, accepted, with no GC.
  - The SockAlias table is per process; the alias count is global.
  - Plain `read()`/`recv()` on a unix stream ignores queued fd batches. That is pre-existing; Firefox only uses recvmsg.
  - The no-fd recvmsg path for unix SOCK_DGRAM/SEQPACKET reads one datagram per iovec. Also pre-existing.
- aarch64/HVF entropy is jitter only (no FEAT_RNG). See the item B notes in the old section below.

### Tools
- `ffsession.py <arch> <tag> [--wait S] [--env "K=V ..."] [--url URL]`:
  - boots `--virgl`, logs in at the greeter, opens cosmic-term with Super+T and runs `/bin/firefox --no-remote URL`;
  - `--url file:///tmp/fftest.html` writes a test page into the guest first;
  - output goes to `$FFSESSION_OUT/run-<tag>/`: ff.log, screenshots every 30 s, serial-live.log and ps.txt.
- `runtests.py <arch> <tag> <cmd>...` is the headless root-login test runner. The 13-suite list is `/bin/sigtest /bin/sigtest2 /bin/memtest /bin/scmtest /bin/polltest /bin/forktest /bin/exectest /bin/pthreadtest /bin/epolltest /bin/timertest /bin/jobtest /bin/waittest /bin/sigchldtest`.
- Run both with `LEANDROS_QEMU_MEM=4G` from the worktree root.
  - Give each concurrent QEMU its own `LEANDROS_RUN_ID` and `LEANDROS_VNC_PORT`.
  - **Two QEMUs of the same arch cannot run at once** (image write lock).
  - **Never run build-all.sh while a QEMU of this tree is up**: it rewrites the images under it.
- Locating a userland crash: temporarily print `as_.regions` for ELR/LR in the EL0 fault path. The VMA's `file_off` gives the file offset; for libxul text, vaddr = file offset + 0x10000. Disassemble with `llvm-objdump --start-address`, and read MOZ_CRASH reason strings straight from the file.

## Previous RESUME HERE (2026-09-28, superseded)
Branch `lane/firefox`, worktree `.claude/worktrees/agent-a40da30690bad16e4`. Not merged, not pushed. The tree is clean. Commits after `b88f48b5`:
- `b97e2526` random: ChaCha20 CSPRNG for getrandom and /dev/urandom (item B)
- `062f852` ports/firefox: minimal PNG icon theme and MIME database (item A)
- a tools/notes commit: `artifacts/notes/lane-firefox-tools/`

Tools:
- `ffsession.py <arch> <tag> [--wait S] [--env "K=V ..."]` boots `--virgl`, logs `leandro` in at the greeter, opens cosmic-term with Super+T and runs Firefox. It saves screenshots, ff.log and the serial logs to `$FFSESSION_OUT/run-<tag>/` (default `/tmp/ffsession`).
- `runtests.py <arch> <tag> <cmd>...` is a headless boot + root login + test runner.
- Run both with `LEANDROS_QEMU_MEM=4G` from the worktree root. `firefox-window-aarch64.png` is the proof screenshot.

**Item B (CSPRNG getrandom): DONE, partly verified.**
- `sched/src/random.rs`: ChaCha20 with fast key erasure, BLAKE2s seed extraction, and reseeding every 1 MiB or 60 s. getrandom, /dev/urandom and /dev/random all use it, and the Linux flag semantics are implemented.
- Sources per arch/accelerator, from the boot log:
  - x86_64/TCG `-cpu max`: `hardware=RDSEED (64 bytes)` + jitter (31 distinct low bytes / 4096).
  - aarch64/HVF `-cpu host` (Apple M4): `hardware=none` — no FEAT_RNG is exposed, so it runs on jitter only (26 distinct low bytes; CNTVCT is 24 MHz).
  - aarch64 TCG `-cpu max` and x86_64 KVM have not been run; both should report RNDRRS and RDSEED respectively.
- Verified: pthreadtest (distinct + quality: chi2 266 on aarch64, 283 on x86_64, no repeats, flags, /dev/(u)random) passes on both arches.
- **Still to do for B:** the full regression suite (sigtest… sigchldtest, see below) and a desktop boot with this commit on both arches. Use `runtests.py <arch> <tag> /bin/sigtest /bin/sigtest2 /bin/memtest /bin/scmtest /bin/polltest /bin/forktest /bin/exectest /bin/pthreadtest /bin/epolltest /bin/timertest /bin/jobtest /bin/waittest /bin/sigchldtest`.
- Open idea: the aarch64 HVF entropy is weak. Candidates are a virtio-rng device plus driver, or EFI_RNG_PROTOCOL. No DTB reaches the kernel on the UEFI path, so `/chosen/rng-seed` is unavailable.

**Item A (icon theme): DONE for aarch64, x86_64 not yet run.**
- GTK no longer aborts. **On aarch64/HVF virgl a Firefox window appears and draws its browser chrome** (tab strip, URL bar, toolbar, the "security features" notification bar) with hardware WebRender (virgl GLES 3.1 on ANGLE/Vulkan/Apple M4).
- Small magenta rendering artifacts appear around the back/forward buttons and at bar edges.
- **The content area is blank:** every content process (types web, extension, privilegedabout) exits with **status 127** (`process_watcher_posix_sigchld.cc:126`).
- **Next step (this was in progress):** find why the children exit with 127.
  - The temporary execve logging (removed, not committed) showed **no execve from the Firefox parent for them**, only glxtest's. A capture-gap caveat applies: `serial-live` held only ~16 lines in that run.
  - **Hypotheses:**
    - (1) Firefox 136's fork server: children are forked, not exec'd, and something in the forkserver/child path `_exit(127)`s. Test with `--env "MOZ_DISABLE_FORKSERVER=1"`, or pref `dom.ipc.forkserver.enable=false` in `ports/firefox/leandros-prefs.js`.
    - (2) The exec goes through posix_spawn / clone(CLONE_VM|CLONE_VFORK) and fails before execve (musl `_exit(127)` on a pre-exec failure). Log clone flags and exit codes for Firefox's children.
    - (3) The execve fails with ENOENT on a bad path from `/proc/self/exe`. The kernel does not log ENOENT.
  - Useful log: `--env "MOZ_LOG=ProcessLaunch:5,ForkServer:5,Process:5"` (it showed only "Launching new process immediately for type …" then 127).
- After that: load `about:` pages or a `file://` page. Networking is untested.
- Then rerun on x86_64. `ports/firefox/out/*` are already restaged with the theme, and the x86_64 image must be rebuilt by `./scripts/build-all.sh`.

Branch `lane/firefox` (local, not pushed, not merged), base `7b28aa05`. Commits:
- `8ba66dca` ports: stage Alpine's prebuilt Firefox as an optional image component
- `89c8a7df` signal: reset the alternate signal stack on execve, inherit it on fork
- `35586d17` aarch64: let EL0 read CTR_EL0 and run cache maintenance (UCT, UCI, DZE)
- `c8799608` mm: demand-paged mappings up to 64 GiB with sparse frame tables
- `3352d1fb` net: answer FIONREAD and TIOCOUTQ on sockets
- `4fb77531` kernel: getpid/getppid name the process; getrandom never repeats
- (this note)

## Verdict
- **Build and staging: DONE on both arches.** `build-all.sh` stages Firefox the way it stages doom and MAME. It is skipped with a `⚠️` warning when docker/podman is missing, the daemon is down or the build fails. If the output is newer than the port scripts it takes under a second.
- **Boot: no regression.** Both arches boot to the COSMIC desktop with Firefox in the image. The greeter login, panel, dock and cosmic-term all work. aarch64 was tested on HVF, x86_64 on TCG, both with virgl on the Mac GPU QEMU (gles31) and 4 GiB of guest memory.
- **Runtime: Firefox gets to hardware WebRender on both arches, then aborts in GTK.**
  - It starts, the Wayland proxy works, and the IPC I/O thread comes up.
  - WebRender initializes on the GPU: `Renderer: virgl (ANGLE (Apple, Vulkan 1.1.357 (Apple M4 Max …)))`, `OpenGL ES 3.1 Mesa 25.3.6`.
  - It then dies in GTK: `Gtk:ERROR:gtkiconhelper.c:495:ensure_surface_for_gicon: assertion failed: Icon 'image-missing' not present in theme Adwaita` → `mozalloc_abort` → exit 139.
  - That is a packaging gap in this port (no icon theme and no SVG pixbuf loader), not a kernel bug. The lane stops here as instructed.

## What ships (port)
- `ports/firefox/build.sh <arch|all>` runs on the host. It picks podman, else docker, and runs `alpine:3.21` with `--platform linux/arm64|amd64`. It follows the conventions of `ports/mesa/build-gpu-stack.sh`: a snapshot of the scripts, a per-arch log, and `=== rc=N arch=A ===` as the last line. On success it touches `out/<arch>/.stamp`.
- `ports/firefox/build-in-alpine.sh <arch>` runs in the container:
  - `apk add firefox` plus fonts, fontconfig, gdk-pixbuf, gtk3, nss and pciutils-libs.
  - Walks the DT_NEEDED closure of every ELF under `/usr/lib/firefox` and the pixbuf loaders. It also adds the libraries Firefox loads with dlopen: the NSS PKCS#11 modules, libpci, libgcc_s, libepoxy, libxkbcommon and libwayland-cursor.
  - Stages everything under the soname, with symlinks dereferenced.
- **Not shipped:** Alpine's Mesa (EGL, GLES, GL, gbm, glapi, gallium), libdrm, libvulkan, libwayland-{client,server,egl} and libudev. These always come from the image.
- **ELF fixes:**
  - `libc.musl-<arch>.so.1` is rewritten to `libc.so`.
  - PT_INTERP stays `/lib/ld-musl-<arch>.so.1`, which is where the image already packs libc.so.
  - `libleandros_ssp.so.1` is added to DT_NEEDED of every ELF that imports `__stack_chk_guard`: 98 ELFs on aarch64, 0 on x86_64.
- **libscudo is kept again** since `c8799608`. It was stripped in the first commit because its 512 MiB `.bss` hit the old 256 MiB file-mmap cap.
- **Symbol audit (`out/<arch>/SYMCHECK.txt`)** checks against the guest libc, the staged tree and the image's own copy of each clashing soname. Result: **0 unresolved on both arches.**
- **Launcher `/bin/firefox`** (`ports/firefox/firefox.sh`):
  - Sources `/bin/gpu-env` and refuses (exit 78) without zink or virgl. GPU rendering only.
  - Sets `MOZ_ENABLE_WAYLAND=1`, `GDK_BACKEND=wayland` and `MOZ_ACCELERATED=1`, and disables every sandbox, the crash reporter, the a11y bridge, dconf and portals.
  - The `MOZ_DISABLE_WAYLAND_PROXY=1` workaround was removed in `3352d1fb`.
- **Prefs** (`/usr/lib/firefox/defaults/pref/leandros-prefs.js`): force hardware WebRender with no software fallback, one content process, no fission, and no network on startup.
- **mkfs** overlays `ports/firefox/out/<arch>/` only if `libxul.so` is present. Where a `usr/lib` soname clashes, the image's copy wins.

| | aarch64 | x86_64 |
|---|---|---|
| Firefox | 136.0.4-r0 (Alpine 3.21.7 community) | 136.0.4-r0 |
| staged tree | 287 MB | 291 MB |
| F2FS image without → with Firefox | 2580 → 3152 MB (+572) | 2588 → 3168 MB (+580) |

## Kernel fixes, root causes

### 1. Silent 139: stale alternate signal stack (`89c8a7df`)
Temporary instrumentation on the frame-write failure showed the failing thread's alt stack was `0xD9CCF000`/12 KiB. **No VMA contained it, and the Firefox process had never called sigaltstack for it.**
- Rust std installs a main-thread alt stack in brush, which runs `/bin/firefox`.
- `execve` did not reset it. So Firefox's first SA_ONSTACK signal (SIGILL/SIGSEGV) built its frame at an address from the discarded image.
- The frame write failed and the kernel turned it into a bare SIGSEGV.

The frame write itself was fine: it already prefaults like a user write, and a fresh untouched mmap'd alt stack works (tested). The fix:
- execve resets the alt stack to SS_DISABLE, as Linux does.
- fork now inherits it; it used to start disabled, which does not match Linux.

Tests, sigtest (`sigtest altstack` runs them alone):
- `altstack_on_fresh_mmap`;
- `altstack_inherited_by_fork`, which failed before (exit 66);
- `altstack_reset_on_exec`, which failed before (exit 71).

### 2. aarch64 EL0 cache maintenance (`35586d17`)
SCTLR_EL1 now sets **UCT** (CTR_EL0 reads), **UCI** (DC CVAU/CVAC/CIVAC/CVAP and IC IVAU by VA) and **DZE** (DC ZVA), exactly as Linux does:
- All three act with the caller's own permissions. UCT is a read-only ID register; UCI needs read access to the VA; DC ZVA is a store.
- DC IVAC stays EL1-only.
- APs inherit the BSP's SCTLR through `smp_init`.

Cache-maintenance aborts (ISS.CM) are now served as reads. Otherwise DC CVAU on a not-yet-faulted RX page would be refused as a write.

Test: memtest `el0_cache_maintenance` reads CTR/DCZID, runs DC ZVA, flushes untouched RW and RX pages, and runs a real JIT write → RX → flush → call sequence.

### 3. Big lazy mappings (`c8799608`)
- **Caps:** anonymous and private-f2fs-file mappings may now be up to **64 GiB**. The paths that populate at map time (device, shared tmpfs VMO, eager copy) keep 256 MiB.
- **Layout:** mmap bumps a system-wide cursor from 0x4000_0000 toward the sigreturn page at 0x7fff_ff00_0000, about 128 TiB on both arches.
- **Verified lazy at map time:** one VMA record, with no page tables or frames.
- **Hidden cost found and fixed:** `lazy_pages` was a dense `Vec` grown to the highest faulted index. One touch at the top of a 2 GiB reservation cost 4–8 MiB of physically contiguous kernel heap from the fault path.
- **Replacement:** `mm::pagevec::PageVec` uses 512-slot chunks, each a Vec grown only to its highest written slot. Small VMAs cost the same as before. Sparse ones pay at most 4 KiB per touched chunk, plus 24 B of directory per 2 MiB of span.
- Unmap, drop, fork, mprotect(PROT_NONE) and the smaps census now walk only present frames.

Test: memtest `big_lazy_reservations`:
- the 2 GiB JIT reservation plus a MAP_FIXED commit at its top;
- 4 GiB touched at both ends, with RssAnon going 76 → 88 KiB;
- a 300 MiB private file map.

### 4. Wayland proxy EBADF (`3352d1fb`)
Firefox's proxy calls `ioctl(fd, FIONREAD)` before every relay read and treats failure as a dead connection. `sys_ioctl` sent FIONREAD for socket fds to the VFS, which does not know them and returned **EBADF**.

The net server now answers FIONREAD and TIOCOUTQ (NET_QUEUE_LEN):
- AF_UNIX: ring byte counts;
- TCP: smoltcp queues;
- UDP FIONREAD: size of the next datagram.

The proxy now works: no ProxiedConnection errors, and GTK gets its Wayland display through it. Test: scmtest `socket_fionread`.

### 5. Found beyond the four (small, kernel-side, fixed in `4fb77531`)
- **getpid returned the thread id.**
  - Symptom: `MOZ_RELEASE_ASSERT(mMyProcInfo == EndpointProcInfo::Invalid() || mMyProcInfo == EndpointProcInfo::Current())` in the IPC I/O thread (libxul file offset 0x2884B54).
  - Cause: an endpoint created on the main thread was bound on another thread, and the two threads' getpid() values differed.
  - Fix: getpid is now the tgid, and getppid is the parent process's tgid.
  - Test: pthreadtest `thread_getpid_is_process`.
- **getrandom returned identical bytes within a scheduler tick.** It reseeded an LCG from the 100 Hz tick on every call.
  - Symptom: Firefox's `Oops: ERROR_PORT_EXISTS` from duplicate 128-bit port names.
  - Fix: SplitMix64 over a global counter plus the ns clock. It is not cryptographic.
  - Test: pthreadtest `getrandom_distinct`.

## Test results (release, final build, headless no-GPU boot, as root)
aarch64/HVF and x86_64/TCG: **13/13 RC=0 each**. The suites were sigtest, sigtest2, memtest, scmtest, polltest, forktest, exectest, pthreadtest, epolltest, timertest, jobtest, waittest and sigchldtest, including all the new checks.

Desktop, both arches (virgl, greeter login as leandro, cosmic-term): the COSMIC desktop comes up normally, and Firefox is launched from cosmic-term.

## Firefox status per arch (final build)
Both arches follow the same path:
- The proxy works.
- IPC is up. The remaining benign warnings are `read-only dup failed (No such file or directory); not using memfd` and `Unable to determine pipe buffer size: Protocol not available`.
- WebRender runs on **virgl GLES 3.1 in hardware**.

Then GTK aborts on the missing `image-missing` icon → exit 139.

x86_64 also logs `Failed to create EGLContext!: 0x3009` (EGL_BAD_MATCH) once before WebRender succeeds.

## Open / next
1. **Port (next blocker, not kernel):** ship an icon theme GTK can load.
   - Alpine's `adwaita-icon-theme` is 13.6 MB, 10 MB of it cursors. Its `image-missing` is SVG only, so it also needs librsvg's gdk-pixbuf SVG loader.
   - The alternative is a tiny PNG theme that provides `image-missing`, `open-menu` and the few icons Firefox's GTK widgets request.
   - Update the pixbuf `loaders.cache` either way.
2. Investigate the `Failed to create EGLContext: 0x3009` on x86_64, probably a context attribute Mesa virgl rejects. WebRender recovers.
3. `memfd` read-only dup via `/proc/self/fd/N` fails with ENOENT: Firefox's SharedMemory_posix reopens the memfd O_RDONLY through `/proc/self/fd`. Firefox has a fallback.
4. `F_GETPIPE_SZ` returns ENOPROTOOPT, which is benign.
5. getrandom is not cryptographic. Seed from RNDR/RDRAND where available.
6. The mmap VA cursor is system-wide and never recycled. 128 TiB lasts a long time, but wasm memories reserve ~8 GiB each.
7. The image-sonames list in `ports/firefox/build.sh` mirrors mkfs's `usr_lib_files`. Keep them in sync.
