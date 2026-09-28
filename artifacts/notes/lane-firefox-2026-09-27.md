# Lane firefox — 2026-09-27

## RESUME HERE (paused 2026-09-28)
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
