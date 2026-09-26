# Lane macqemu, 2026-09-25

Branch `lane/macqemu`, cut from `origin/lane/gpudefault` (a03f1c1). Mac worktree: `~/code/leandros-macqemu`. The scratch test tree `~/code/leandros-macqemu-test` is a03f1c1 plus a snapshot of macgpu's uncommitted kernel fix. It is not committed anywhere and can be deleted.

## What was built

`~/.local/qemu-gpu` holds a standalone QEMU for the Mac with GPU rendering, driven from the CLI. It does not use Homebrew's qemu, and it does not use UTM's app or binaries at runtime. `scripts/mac-qemu-gpu/build.sh` rebuilds it from pinned sources and verifies checksums. A clean rebuild takes about 10 min on the M4 Max; it was verified end to end into a scratch prefix. Xcode is not required; the Command Line Tools are enough.

- **ANGLE**, Metal and GL backends, built from source at rev 72b8f72, the same revision and dependency set as MacPorts `angle` 2.1.28727. It uses gn, with no depot_tools and no Xcode.
  - I made two build fixes (`patches/angle-clt-no-xcode.patch`). First, `sdk_info.py`/`find_sdk.py` now accept a CLT SDK. Second, Metal internal shaders are compiled at runtime; the offline `metal` compiler needs Xcode's Metal toolchain.
- **libepoxy 1.5.10**, patched to enable EGL on macOS. It dlopens ANGLE by absolute path, so no `DYLD_*` variables are needed.
- **virglrenderer 1.3.0** (upstream), patched with an eventfd stand-in: an O_RDWR FIFO. Without it, `THREAD_SYNC` is dropped on macOS and every fence waits for QEMU's 10 ms poll timer. The effect shows in drmsmoke `FLIP_EVENT_DELIVERED_ON_FENCE`: macgpu measured 17/32 on the unpatched build, and I measured 32/32 with the patch.
- **QEMU 11.1.1** with HVF, `aarch64-softmmu` and `x86_64-softmmu`, and one patch to `ui/egl-helpers.c`:
  - an ANGLE EGL display on macOS, so `egl-headless` works without GBM;
  - `GL_BGRA_EXT` as the internal format of the readback texture. Strict ES rejects an RGBA texture fed BGRA data, which made **every** egl-headless frame black, including the UEFI screen.
- The build snapshots the CLT SDK into `$WORK/sdk`. During this lane `/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk` disappeared and reappeared twice, and each time a build that straddled the change broke ("library 'System' not found").

## Wiring

- `run-qemu.sh`: on the Mac it auto-detects `~/.local/qemu-gpu`; `LEANDROS_QEMU_PREFIX` overrides it on any host, including the firmware. `--gpu auto` then resolves to **virgl**, using `egl-headless` plus `-vnc` (`LEANDROS_VNC`, default :0), and the script prints the `vnc://` URL. On Darwin, auto never picks Venus.
- `driver.py`:
  - uses the same prefix;
  - adds `--virgl` (or `LEANDROS_GPU=virgl|auto`), which gives aarch64 `virtio-gpu-gl-pci,id=virglgpu` and x86_64 `virtio-vga-gl`, with VNC on that console;
  - `screenshot` grabs pixels over VNC with pure-Python RFB, because a GL scanout has no screendump surface;
  - `--venus` is allowed on the Mac only when the QEMU has `venus=`;
  - fixes a bug where the x86_64 `virgl` local variable shadowed the flag.

## Evidence

All results are from aarch64/HVF on the M4 Max unless the row says otherwise.

| check | result |
|---|---|
| guest GL_RENDERER | `virgl (ANGLE (Apple, ANGLE Metal Renderer: Apple M4 Max, …)) \| OpenGL ES 3.0 Mesa 25.3.6`, on both aarch64/HVF and x86_64/TCG |
| greeter pixels over VNC, with the macgpu kernel fix | renders correctly (`/tmp/mq-greeter4.png`) |
| greeter pixels, a03f1c1 kernel | black, plus `Illegal resource 16`. This is the guest bug macgpu fixed in `ee7e01c` |
| greeter keystroke latency (12 keys, VNC, includes the 30 ms hold) | p50 **0.069 s** (0.032–0.091) with the FIFO patch; before it, p50 0.085 s |
| drmsmoke, greeter stopped | failed=0, skipped=0, flips on fence 32/32, d_timeouts=0 |
| COSMIC session (login `leandro`) | wallpaper renders; **no panel/dock**, which macgpu also sees on laptop virgl |
| session stability | QEMU stays alive past 15 min. The **guest OOMs at about 13.5 min**, reproduced twice: `greetd.log` reached 8.4 M lines from a `cosmic-panel … Protocol error 7 on object @0` loop, used memory grew about 88 MiB/min, and `[BUDDY] Allocation failed`. The cause is in the guest, not the host |
| greeter-only run, 15 min | stable. QEMU RSS flat at about 667 MB, host CPU 2–13 %, guest used 322→323 MiB, the frame keeps updating (clock), 0 OOM |
| run-qemu.sh aarch64 / x86_64 | both boot with `🎮 GPU path: virgl` and the console visible over VNC |

Host CPU during the session is about 4 vCPUs × 95 %, all of it spent inside the guest (`hv_trap`). That is the panel loop above.

## Venus on the Mac: not working, and not cheap

- Upstream virglrenderer Venus is Linux-only: it uses epoll, memfd, udmabuf and eventfd.
- UTM's fork (`utmapp/virglrenderer` 5d26f605) ports vkr to macOS and builds against Homebrew `molten-vk` + `vulkan-loader`. The `build.sh venus` stage (experimental, installs to `$PREFIX-venus`) gets this far:
  - QEMU exposes `venus=`;
  - the guest sees capset 0x16 (VENUS=1), creates a Venus context, and maps its ring blobs.
- It fails at the host render server: `vkr: vkCreateInstance resulted in CS error`, so vktest and the zink probe abort. The cause is undetermined. The two candidates are a protocol/pNext mismatch between guest Mesa 25.3.6 Venus and the fork, and hostmem coherence under HVF. KosmicKrisp was not tried.

## Open
- The panel loop and log spam under virgl; macgpu owns the virgl guest path.
- Venus on the Mac: the CS error above.
- The desktop and laptop have not been touched.
