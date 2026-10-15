# Building

[`scripts/build-all.sh`](../../scripts/build-all.sh) builds everything — the kernel twice per architecture, the in-tree userland, relibc, the ported software — and writes three disk images per architecture. Always build **release**; there is no supported debug build.

## Usage

```sh
./scripts/build-all.sh                       # both architectures (default)
./scripts/build-all.sh --arch aarch64        # one architecture (aarch64 | x86_64 | both)
./scripts/build-all.sh --arch aarch64 --rpi5 # Raspberry Pi 5 kernel features
./scripts/build-all.sh --raspi4b             # QEMU raspi4b / SDHCI test path
./scripts/build-all.sh --arch x86_64 --nvidia # x86-64 kernel with the OpenRM NVIDIA driver
./scripts/build-all.sh --help
```

`--nvidia` builds `libnvrm.a` first (`ports/openrm/build.sh`, which fetches the pinned OpenRM source and GSP firmware on first use) and enables the kernel's `nvidia` feature; see [PC hardware](../hardware/pc.md). `--rpi5` selects the BCM2712 UART, GIC and MMU addresses and links the direct-boot kernel at the Pi firmware's load address; see [Raspberry Pi 5](../hardware/raspberry-pi-5.md).

A full build from clean takes from several minutes on a fast machine to most of an hour on a small laptop; incremental builds are much shorter. The container-staged ports are the slow first step and are skipped while up to date.

## What the build does

Once per run it downloads and caches the Limine bootloader binaries (version 11.4.1, in `.limine-cache/`), builds the input-stack shims (`libseat`, `libudev`, needs zig) and, when `../doomgeneric` exists, stages the SoundFont. Then, for each architecture:

1. **relibc** (`../relibc`), with its C sources pointed at the zig musl wrapper.
2. **In-tree userland** (`scripts/build-userland.sh --release`): the programs and regression suites under `userland/`, plus the two C conformance tests (`privtest`, `nettest`) when zig is present.
3. **Ported software**: DOOM, MAME, Firefox, the desktop portal, PipeWire, `bottom`, brush, coreutils, mkfs.fat, mkfs.xfs, `spawnwedge`, `killmt`, disks-rs's `disktester`, then the D-Bus session package (busd, `dbus-run-session`, `session.conf`), the system-bus services (`ports/sysbus`) and the iso-codes data.
4. **initrd** — a `newc` CPIO holding only `/bin/init`; everything else comes from the root file system. It is written uncompressed (`initrd-<arch>.cpio`) and copied to `initrd-<arch>.cpio.gz`, because the kernel's initrd parser does not decompress.
5. **Kernel**, built twice with `-Z build-std=core,alloc` against the custom target spec `targets/<arch>-unknown-kernel.json`:
   - the **standard** kernel, linked with `linkers/<arch>.ld`, booted by Limine → `target/final-<arch>/kernel`;
   - the **direct-boot** kernel, linked with `linkers/<arch>-direct.ld` → `target/final-<arch>/kernel-direct`, plus a flat binary `kernel-direct.bin` (and, on x86-64, a 32-bit ELF `kernel-direct-32.elf`) when `llvm-objcopy`/`rust-objcopy` is installed.
6. **Boot image** `leandros-limine-<arch>.img` — 512 MiB, one GPT EFI system partition (FAT32, label `LEANDROS`) holding Limine's EFI loader, `kernel.elf`, the initrd and [`limine/limine.conf`](../../limine/limine.conf). On x86-64 Limine's BIOS stage is installed as well, and a `.vdi` copy is written when `VBoxManage` exists.
7. **Root volume** `f2fs-data0-<arch>.img`, written by [`scripts/mkfs-f2fs-populated.py`](../../scripts/mkfs-f2fs-populated.py): the full userland — shell, coreutils, test binaries, Mesa, the COSMIC session set, Firefox, accounts in `/etc/shadow`. It is sized from its content and then doubled, so it ships about half free (roughly 3.5 GiB per architecture with everything staged).
8. **Home volume** `f2fs-data1-<arch>.img` — the persistent `/home` (f2fs label `leandros-home`, written by [`scripts/mkfs-f2fs-home.py`](../../scripts/mkfs-f2fs-home.py)): 8 GiB sparse, seeded with the home skeleton (`/home/leandro`, `/home/cosmic-greeter`). Created only when missing and **never rewritten by a build**, so Claude Code credentials, the Firefox profile and COSMIC settings survive rebuilds. An old-layout data1 (a clone of the root, mounted at `/data` before 2026-10-15) is replaced once, with a message. `--reset-home` (or `LEANDROS_RESET_HOME=1`) recreates it.

### Two build gates

- **Kernel stack frames.** Both kernels are linked with `-Z emit-stack-sizes`, and [`scripts/check-stack-frames.py`](../../scripts/check-stack-frames.py) fails the build if any function's frame exceeds **48 KiB**. Kernel stacks are 128 KiB with no guard page; an oversized frame does not fault, it overwrites the neighbouring buddy allocation and the corruption surfaces somewhere else entirely. A 156 KB frame in `f2fs::mount` (and a 72 KB one in `sys_umount2`) once spent a day looking like a host-dependent boot failure. The largest legitimate frame is about 34 KB (`vfs_close_all_for`, inflated by LTO inlining). See [scheduler](../kernel/scheduler.md).
- **Stale staging.** Every payload the image takes from a port is regenerated from tracked source on each build, and the image builder refuses to write an image whose staged D-Bus payload is older than its source (`verify_dbus_staging`). A `.service` activation once sat in the repository and in no image for two days while a boot test that only asked "does the desktop come up" called it green. A failing port step warns in `build-all.sh` and the image step refuses: never silent in either place.

## Release builds only

Always build release targets. Debug builds hang or crash early in boot — they need far larger kernel stacks than the release profile, and their symbols desynchronise from the running image. Nothing in the scripts produces or tests a debug kernel, and no result obtained on one should be trusted.

## Container-staged ports

Firefox, the xdg-desktop-portal frontend and PipeWire/WirePlumber are repackaged from Alpine 3.21's prebuilt packages inside a container of the target architecture (Firefox is about 290 MB of image content). The first staging takes minutes, because the foreign architecture runs under emulation; afterwards a port is skipped while its `ports/<name>/out/<arch>/.stamp` is newer than the port's scripts.

When no container tool answers within `LEANDROS_CT_TIMEOUT` seconds (default 20), or the staging run fails, the build reuses a previously packaged tree: this worktree's `ports/<name>/out/<arch>`, then the main checkout's, then `$LEANDROS_ARTIFACTS/<name>-out/<arch>`. With none of those it stops loudly — an image silently missing its audio stack or portal is worse than a clear error.

| Variable | Effect |
|---|---|
| `LEANDROS_SKIP_FIREFOX=1`, `LEANDROS_SKIP_PORTAL=1`, `LEANDROS_SKIP_PIPEWIRE=1` | Build an image without that component |
| `LEANDROS_FIREFOX_REBUILD=1`, `LEANDROS_PORTAL_REBUILD=1`, `LEANDROS_PIPEWIRE_REBUILD=1` | Force the container run even when up to date |
| `LEANDROS_PORTAL_BACKEND=<file>` | Use a prebuilt `xdg-desktop-portal-cosmic` on machines that cannot cross-build it |
| `LEANDROS_CT_TIMEOUT=<s>` | Container probe timeout |
| `LEANDROS_RELIBC_DIR`, `LEANDROS_BRUSH_DIR` | Build relibc / brush from another checkout |
| `LEANDROS_BRUSH_UNCHECKED=1` | Build an off-pin brush (local brush development) |

The brush pin check exists because an unpatched `../brush` once shipped a `/bin/sh` whose `cmd &` was still an unstarted in-process task when the next command ran, so `daemon & exec session` dropped PipeWire from the session — on one architecture only, because only one machine's checkout lagged. Fix a mismatch with `ports/brush/sync.sh apply ../brush`.

## SoundFont

When the DOOM sibling checkout is present, the build downloads the pinned [Fluid R3 3.1 SoundFont source archive](https://deb.debian.org/debian/pool/main/f/fluid-soundfont/) (about 129 MiB) for General MIDI music. The 142 MiB `FluidR3_GM.sf2`, its original MIT licence (`COPYING`) and attribution (`README`) are cached under `${XDG_CACHE_HOME:-$HOME/.cache}/leandros/soundfonts` and installed in `/usr/share/soundfonts` on both architectures. Cached files are SHA256-verified; later builds need no download. To stage them separately, run `python3 scripts/soundfont.py`.

## Image provenance pitfalls

The images are build outputs, and `run-qemu.sh` boots whatever the last build wrote, whatever the checkout now says. Several hours have been lost to booting the wrong bits:

- **Board builds overwrite the virt image.** `--rpi5` and `--raspi4b` write the same `leandros-limine-aarch64.img` path as a normal build. Booting that image on QEMU `virt` faults before producing any output at all — the kernel dies setting up a UART that does not exist on `virt` (`X19 = 0xffff0000fe201000`, the BCM PL011 base). **If an AArch64 `virt` boot is silent, check image provenance first** and rebuild without the board flag.
- **A stale image after a pull.** A checkout moved forward without a rebuild boots old code. `run-qemu.sh` now prints `STALE IMAGE` when the boot image is older than the last commit touching kernel, server, userland, port or image-building sources. (A 16-September image, still softpipe-only, once cost 5–30 s per greeter keystroke and was read as a viewer problem.)
- **Shared siblings are shared state.** `../doomgeneric` is one directory for every worktree on a machine; a binary built there is owned by whichever tree linked it last. The build therefore uses a per-worktree, per-architecture object directory, links atomically, and takes DOOM from this tree's `target/doom/`. If DOOM misbehaves, check that the build log says `doom: target/doom/doom-<arch>`.
- **The root is rebuilt, `/home` is kept.** Every build rewrites `f2fs-data0` (anything written to the root is lost) and keeps `f2fs-data1`, the `/home` volume. `./scripts/build-all.sh --reset-home` starts `/home` over. Automated runs boot a throwaway clone of it. See [testing](../development/testing.md#fresh-images).
- **Never rebuild while a QEMU booted from the same tree is running** — the build rewrites the images under it.
- **Manually injected files do not survive.** A rebuild regenerates the initrd and every image, wiping anything copied in by hand.

For running several builds in parallel (one per worktree), see [workflow](../development/workflow.md#build-outputs-and-image-provenance).

## See also

- [Prerequisites](prerequisites.md)
- [Running](running.md)
- [Conventions](../development/conventions.md) — release-only, Limine revision 6.
- [Boot](../kernel/boot.md) — what the kernel does with these images.
