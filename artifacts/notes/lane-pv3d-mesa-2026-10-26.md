# lane/pv3d-mesa 2026-10-26 (openbugs #98 phase F): auto-arm, Pi Mesa ship-set, GPU smoke tests

Status: DELIVERED (code, host tests, QEMU guards). Pixels and the PASS lines need a Pi boot from a card
prepared AFTER this merge (the sysfs identity and the new binaries live on the root partition).

## What landed
- **Auto-arm** (`drivers/src/v3d/mod.rs`, `submit.rs::auto_arm_decision`, `drm_device_interface.rs`):
  after a successful probe AND an MMU that came up, the probe calls `v3d_arm_hw()` and logs
  `[v3d] armed: card0+renderD128 report v3d (auto, probe ok)`. `v3d.arm=0` on the boot line keeps it
  disarmed (`[v3d] not armed mmu=up (...)`). The arm is LATCHED: a later `SET_CLIENT_CAP 0x1000_0003 = 0`
  (the regress guard `drmsmoke --arm-v3d` always disarms on exit) is ignored on real hardware
  (`[DRM] v3d disarm ignored (real hardware, latched)`). QEMU virt: the probe is `rpi5`-only, so nothing
  auto-arms; `--arm-v3d` works as before.
- **Mesa** (`ports/mesa/build-gpu-stack-alpine.sh`): aarch64 now builds `-Dvulkan-drivers=virtio,broadcom,nouveau`
  (v3d was already in the Gallium megadriver). The stage gains `libvulkan_broadcom.so` +
  `broadcom_icd.aarch64.json`; `libvulkan.so.1` (Khronos loader) was already shipped.
  Container-side gate: every megadriver/ICD must carry an NT_GNU_BUILD_ID note (exit 7).
- **Packer** (`scripts/mkfs-f2fs-populated.py`): packs the broadcom ICD + json, `/bin/es31test`, `/bin/vkprobe`;
  refuses (fatal) a libgallium / libvulkan_* without a GNU build-id note (#98 tail, the FAR=0x10 crash).
  Checker verified: True for the shipped stages, **False for the 2026-08-20 mesa-v3d-rpi5 libgallium that
  produced FAR=0x10**.
- **Pi DRM identity** (packer, `LEANDROS_BOARD=rpi5`, set by `prepare-rpi5-sdcard.sh`): the image staged
  card0/renderD128 as PCI virtio-gpu 1AF4:1050 on every board. On the Pi that sends Mesa's GL loader to
  `virtio_gpu` (PCI id wins over DRM VERSION) and hides the GPU from v3dv, whose `enumerate_devices`
  accepts only `DRM_BUS_PLATFORM` devices (v3dv_device.c:1569). The Pi card now gets
  `device/subsystem -> ../../../../bus/platform` and a uevent with `OF_FULLNAME=/axi/v3d@2000000`,
  `OF_COMPATIBLE_0=brcm,2712-v3d`, `OF_COMPATIBLE_N=1` (what libdrm's platform parser reads). The
  kmscube2 note's "not a Pi problem (platform device)" was wrong for our image: REFUTED.
  On an already-prepared card it can be fixed in place over SSH (the files are disk-backed).
- **Smoke binaries** (built in the stage container, one PASS/FAIL line each):
  `vkprobe [--want V3D]` (instance via libvulkan.so.1, lists `# gpuN:` like `vulkaninfo --summary`,
  then vkCmdFillBuffer 64 KiB + fence + check), `es31test [--want V3D]` (GLES 3.1 compute, 4096 u32,
  `0xC5D00000|id`, exactly the pv3d-tfu-csd plan), `kmscube` (existing). vulkaninfo/vkcube were not
  shipped: vkcube needs a WSI surface (v3dv WSI wants a display device with a connected connector),
  vkprobe covers device + submit + fence without one.
- **`scripts/pi-gpu-smoke.sh [pi:<ip>]`**: runs ident / armed / vulkan / compute / kmscube through
  `remote.py run`, prints `PASS|FAIL <test> ...` per test and `pi-gpu-smoke: N/5 PASS`.

## Evidence
- Host: `cd drivers/v3d-tests && cargo test --target aarch64-apple-darwin`: 63/63 (new: auto-arm decision).
- `scripts/check-rpi5.sh` rpi5 + raspi4b: clean, no warnings in v3d / drm_device_interface.
- Mesa stage, regress and QEMU guards: see "Results" below.

## Results
- Mesa stage rebuilt on the Mac (Docker, Alpine 3.21 aarch64): `=== rc=0 ===`, `vulkan-drivers: virtio,broadcom,nouveau`,
  libvulkan_broadcom.so 11.6 MB, broadcom_icd.aarch64.json, /usr/bin/{gpuprobe,es31test,vkprobe}, build-id gate passed.
  Installed as `leandros-artifacts/m3-gl-stack/gpu-stage-aarch64` (previous kept as `gpu-stage-aarch64.bak-20261026-pv3dmesa`;
  the new one is a superset: same megadriver config plus the broadcom ICD and two binaries).
- QEMU aarch64 (Mac, HVF, under the qemu lock, image from the new stage): `--arm-v3d` kmscube guard
  `Rendered 29 frames in 0.302 sec (96.1 fps)`; `es31test PASS csd ok 4096/4096 renderer=softpipe` (the test
  itself is sound: softpipe runs the same compute shader); `vkprobe FAIL no-physical-device r=-3` (expected on
  plain virt: no Venus, and v3dv sees no platform device); no `[v3d] armed` in dmesg and gpuprobe does not report
  V3D after the guard disarms, so virt never auto-arms. Overall PASS, 0/1 boots failed.
- Full aarch64 regress (box farm, TCG, lane/pv3d-mesa @ the code commit): 49 suites run, 0 bad, 0/1 boots failed, overall PASS (build 214 s + tests 721 s).

## COSMIC on v3d: desk check (what is missing)
1. **Session policy**: `ports/greetd/data/gpu-env` knows zink / iris / virgl / software only. A `card0=v3d`
   branch is needed (verify `GL_RENDERER` contains `V3D` via gpuprobe, export nothing else); today the
   greeter would refuse the session as "no GPU renderer".
2. **Input**: keyboard/mouse are USB, behind RP1 on PCIe (off in the rpi5 build until pipcie + an RP1
   xHCI lane land). No `/dev/input/event*` = greeter cannot log in. Interim: autologin, or SSH-started session.
3. **Display**: phase E scanout is a CPU copy per flip (`kms_v3d::present`: dc ivac, stride copy into the
   firmware FB, dc cvac): ~8 MB per 1080p frame, no real vblank (flip events are synthetic). Usable, slow;
   the real fix is the gen-6 HVS dlist plane (scanout note). ADDFB2 accepts only linear XRGB8888, pitch w*4,
   no modifier, so card0's primary-plane `IN_FORMATS` must advertise LINEAR only on rpi5 or smithay will
   allocate Broadcom UIF/SAND buffers that ADDFB2 refuses. Unverified: check IN_FORMATS on the Pi.
4. **Buffer sharing**: Wayland clients hand dmabufs to cosmic-comp: PRIME export/import of v3d BOs
   (`PRIME_HANDLE_TO_FD`/`FD_TO_HANDLE` for v3d handles) and the linux-dmabuf feedback main device
   (renderD128 = v3d) must work; not exercised by any test yet (kmscube is single-process).
5. **Seat / DRM master**: same greetd + seat path as QEMU; nothing Pi-specific known, but DRM master
   hand-off between greeter and session has only been run on virtio/i915/nvidia.
6. **Memory**: v3d BOs are buddy-contiguous power-of-two (a 1080p XRGB buffer = 16 MiB); a COSMIC session
   with ~10 surfaces needs a few hundred MiB contiguous. Fine on 4/8 GB, watch fragmentation.

## Next Pi boot (card prepared from main after this merge; no override env)
- Console: `[v3d] armed: card0+renderD128 report v3d (auto, probe ok)` after the self-test lines.
- Mac: `LEANDROS_PI_HOST=<pi-ip> scripts/pi-gpu-smoke.sh` with no desktop session holding card0. Expected:
  `PASS ident subsystem=platform`, `PASS armed auto`,
  `PASS vulkan dev=V3D_7.1.6.4 api=1.3.x fill ok 16384/16384`,
  `PASS compute csd ok 4096/4096 renderer=V3D_7.1.6.4`,
  `PASS kmscube renderer=V3D 7.1.6.4 Rendered 120 frames in ...`, `pi-gpu-smoke: 5/5 PASS`.
- Failure reading: `FAIL ident` = card prepared without LEANDROS_BOARD=rpi5 (old card); `FAIL vulkan
  no-physical-device` with ident PASS = v3dv's VERSION/name check (`v3d` on both nodes) or render node
  missing from drmGetDevices2; `FAIL compute wrong-renderer renderer=softpipe` = GL loader did not pick v3d;
  `FAIL compute mismatch` = CSD ran wrong (dmesg `[v3d]` CSD lines); kmscube FAIL with no frames = DRM
  master taken (log out of the desktop) or scanout.
