# History

A dated timeline of LeandrOS's milestones, with the commits that mark them. The rest of the handbook describes the current design; this page is the one place that is organised by date. Commit ids are on `main` unless a lane branch is named.

## Origins (April–June 2026)

| Date | Milestone | Commits |
|---|---|---|
| 2026-04-12 | Initial scaffold of a Rust microkernel workspace ("LOS"): kernel nucleus, `mm` (buddy/slab/paging/VMM), `sched`, `ipc` (ports, messages, channels), driver framework, x86-64 and AArch64 stubs. | `9792d63` |
| 2026-04-13 | Renamed to Cyanos; USB (xHCI) and Wi-Fi (mac80211) driver skeletons; boot infrastructure for both architectures; context switching and the first syscalls. | `e73ddac` |
| 2026-04-15 | Kernel subsystems completed enough to boot: SMP scheduler with exit-log side table, W^X demand paging, per-CPU SWAPGS syscall stacks, GICv2, PSCI SMP, Limine and DTB parsing, Raspberry Pi 5 boot support. GPL-3.0 licence added. | `4679aa2` |
| 2026-04-17 | The POSIX personality appears: 100+ syscalls, the VFS/net/TTY/proc servers, a PID 1 init with an interactive shell, and the first relibc build. | `a6b1784` |
| 2026-05-01 | Rebranded to **LeandrOS**. Limine boot protocol **revision 6** compliance — the floor the project has kept ever since. | `1ae872a`, `071132d` |
| 2026-05-10 | Framebuffer console with a vector font, KMS with EDID autodetection, the DRM subsystem and server, and **DOOM** on DRM. | `fa422ae`, `4444542`, `dde08c9` |
| 2026-05-15–17 | **Audio**: VirtIO sound driver and the PipeWire server, auxv-based service discovery, the 128 KiB spool that decoupled clients from the hardware ring. | `5008863`, `4b6c50a` |
| 2026-05-22 | VirtIO GPU 2D/3D driver. | `9a7315a` |
| 2026-06-05 | VirtIO block driver and F2FS tooling: persistent storage. | `a5a125d` |

## Becoming a POSIX system (July 2026)

| Date | Milestone | Commits |
|---|---|---|
| 2026-07-01 | pthreads verified on both architectures. | `155c1f0` |
| 2026-07-02 | Real `poll`/`select`/`epoll` readiness. | `9db447f` |
| 2026-07-03 | Networking: smoltcp TCP/IP over virtio-net. | `4641ba5` |
| 2026-07-04 | **4-CPU SMP** with EEVDF scheduling on both architectures. | `795cfdb` |
| 2026-07-05 | Init mounts F2FS and `pivot_root`s into it; the root file system becomes a populated image. | `a739b02` |
| 2026-07-12 | SDHCI SD-card driver for the Raspberry Pi path (tested on QEMU `raspi4b`). | `1b7defc` |
| 2026-07-15 | **MAME** runs (five blocking bugs fixed). | `ec53aad` |
| 2026-07-16 | HVF acceleration becomes the default on Apple Silicon, after non-volatile virtio MMIO writes (an `STP` HVF cannot decode) were fixed. | `9ae5ed1` |
| 2026-07-19–20 | The **brush** shell and **uutils coreutils** ported; tmpfs semantics, symlinks, hardlinks, `chroot`, real `fsync`. | `a1f677f`, `65131da` |
| 2026-07-21 | Login sessions (`/etc/shadow`, real `setresuid`); xattrs and enforced POSIX ACLs; the Wayland + COSMIC plan. | `611bbf8`, `fd814d7` |
| 2026-07-22 | Wayland transport verified (M1); **dynamic linking**: `ET_DYN` at a load bias and `PT_INTERP`, `dlopen` on both architectures (M2); `kmscube` renders animated frames (M3). | `dc83e89`, `fbac196`, `30917b8`, `ba22bee` |
| 2026-07-23 | A Wayland compositor accepts, composites and draws a cursor on both architectures (M4). | `a52b994` |
| 2026-07-25 | **COSMIC desktop milestone**: a panel bar over the Orion Nebula wallpaper on AArch64, pixel-verified, after about 28 kernel bugs over eight days. Non-root sessions and a clean session log follow the same day. | `8a76fa2`, `869328f`, `6a0eb0c` |

## The desktop matures (August 2026)

| Date | Milestone | Commits |
|---|---|---|
| 2026-08-03 | **Vulkan M1**: a host-populated Venus capset; the host decoder sees the guest's command stream. | `04bde83` |
| 2026-08-04 | Vulkan M2: `vktest` opens a real host GPU through the Venus ICD. | — |
| 2026-08-05 | AArch64 kernel FP/SIMD clobber fixed: the kernel was built with NEON while the trap frame saved no vector state, so file-backed faults overwrote a thread's `q0`. The kernel now builds soft-float. | — |
| 2026-08-07 | Vulkan M4 client half: a Vulkan Wayland client presents into `cosmic-comp`. | — |
| 2026-08-08 | Scope decision: `cosmic-greeter` becomes the intended default login; greetd, PAM, utmpx and VT switching move into scope. Upstream greetd drives `cosmic-greeter` to a login screen. | `5fe6b84`, `76fa908` |
| 2026-08-09 | The "`.data` cliff" on one host is closed as a kernel stack overflow (a 156 KB frame on a 64 KiB stack); 128 KiB stacks, a stack canary and the build-time frame budget follow. | — |
| 2026-08-10 | **A terminal runs**: PTYs, six virtual terminals and `cosmic-term` on both architectures; VT round trip at 0.0 % pixel difference. COSMIC sluggishness root-caused to kernel overhead (an uncached LAPIC read per `current_pid()`, a 512-slot epoll walk, a 256-slot `tgid_of` scan): 0.20 → 1.00 fps. virgl OpenGL passthrough reaches a real host GPU. | `98b4a52`…`1a2c24a`, `f7c4577`, `eb9422e` |
| 2026-08-11 | COSMIC composites on Vulkan through Zink (the blocker was a resource-id overlap). | `a32f53d` |
| 2026-08-20 | **Raspberry Pi 5** reaches userspace on real silicon, after five bugs QEMU cannot show by construction. Mesa with the v3d driver builds. | `390eccd` |

## Linux fidelity sweeps (September 2026)

| Date | Milestone | Commits |
|---|---|---|
| 2026-09-15 | **disks-rs runs unmodified**: loop devices, GPT, `BLKPG`, Rust mkfs.fat/mkfs.xfs, three formats succeed on both architectures. | `050df84`…`588504a` |
| 2026-09-15 | Zero permanent COSMIC patches: the `cosmic-session` `env_rx` patch retired. | `55acacb`, `c0bdcd5` |
| 2026-09-16 | **Graphical login by default**, supervised beside the serial getty. The blockers were kernel bugs: `FUTEX_WAIT_BITSET` read as a relative timeout, `CLONE_PARENT_SETTID` ignored (musl threads with tid 0 deadlocked `fork`), and an O(n) buddy `free`. Zink renders 20 fps (it had been rendering zero frames, not slow ones). The guest clock, 5–36 % slow on every accelerator, moves to the hardware counter. `kill -9` of a multithreaded process, permission enforcement, job control and `#!` scripts all land the same day. | `efb3a85`, `9e45285`, `6a313a1`, `8897e02`, `13fc269`, `4c0ef70`, `b53e418`, `8d8edc2`, `e67aeaf`, `503b11e` |
| 2026-09-18 | Wave of ten lanes (time, TSC calibration, brush, permissions, run queue, poll, exec, compositor locks, GPU interrupts). | `fc5a6bc` |
| 2026-09-24–27 | **GPU rendering by default** with a loud refusal of software rendering; lazy copy-on-write fork and lazy private file mmap; targeted TLB shootdowns; one-shot deadline timers; private futexes keyed by (tgid, address); the Mac GPU QEMU reaches GLES 3.1; the `weak_window` leak fix for `cosmic-comp`. | `a5b5b62`, `2f4214bd`, `cbe99478` |

## Applications and real hardware (October 2026)

| Date | Milestone | Commits |
|---|---|---|
| 2026-10-01–02 | Epoll keyed by open file description; framebuffer and fence lifetimes; line-atomic kernel console output (the "lost serial login bytes" were spliced output, not lost input); COSMIC Settings pages, the desktop portal, system-bus services, orderly power-off. | `aff49e1`, `d7d3297`, `6703408`, `7c50ac3` |
| 2026-10-02 | **Firefox** browses http/https (including HTTP/3) on both architectures with hardware WebRender. | `39ca01e`, `e3faafa` |
| 2026-10-03 | Firefox audio through PipeWire; scroll-wheel events. | `0a52161` |
| 2026-10-05 | Root file system on an xHCI USB mass-storage stick, both architectures. | `8177771` |
| 2026-10-07 | DOOM as a Wayland client with PipeWire sound; per-VT DRM master. A real x86-64 PC boots from the stick with its root on it; USB HID keyboard; dense CPU indices and x2APIC; the NVIDIA OpenRM shim. | `be47da2`, `95bd575`, `7bf2d40` |
| 2026-10-08 | On a real RTX 3070, NVIDIA's GSP-RM firmware completes initialisation (`RmInitAdapter succeeded`) on the first attempt. | — |

## See also

- [Overview](README.md) — the current state in one page.
- [Methodology](../development/methodology.md) — the refuted premises that several of these milestones turned on.
- [TODO.md](../../TODO.md) — what is still open.
