# lane/irisseg (2026-10-23): iris probe SIGSEGV on the laptop - FIXED, promoted

P0 from lane/laptop1: on main cd0c41f3 greetd logged `gpu-env: iris probe failed (rc=139 renderer='')`
and refused COSMIC; good kernel deployspeed-legant (= main cbcdf0f0 + iwl fix) was fine.

## Root cause (two merges, each fine alone)

- uaccess2 (618c6cc5, commit 059ecd17): `DrmDevice::handle_ioctl` bounces every ioctl's top-level
  argument struct through a kernel buffer and passes the kernel pointer down, including to the GPU
  UAPI backend (`gpu_uapi::backend().ioctl`).
- smapx86 (deed9432, commit 8fea3dea): the i915 UAPI's `read_user`/`write_user` now go through
  `sched::uaccess`, whose `user_range_ok` refuses any address >= USER_END.
- Together: every i915 private ioctl (GETPARAM, QUERY, GEM_CREATE, MMAP_OFFSET, EXECBUFFER2, ...)
  read an all-zero struct (`unwrap_or_else(zeroed)`) and silently dropped its stores. Mesa iris then
  crashed in userspace on garbage results -> SIGSEGV with no kernel fault line (it was not a kernel
  fault at all). Not reachable in QEMU regress: no test ran the i915 UAPI.

## Fix (94661aaa)

- `UapiBackend::user_args`: the backend does all access to its argument through `sched::uaccess`.
  i915 = true, NVIDIA = false (its uapi.rs still reads the argument struct raw, so it keeps the bounce).
- `handle_ioctl` skips the bounce for nr DRM_COMMAND_BASE..0xA0 when such a backend is registered and
  v3d is not active (KMS ioctls 0xA0+ that the backend declines still get the bounce).
- runtests: `--cmdline "ARGS"` (boots a cp -c copy with set-image-cmdline.sh) and suite `i915fake`
  (`/bin/i915test`, implies `i915.fake=2 i915.kms=1`).

## Evidence

- QEMU x86_64, fake i915, unfixed main 612d0f90: i915test fails=38 (getparam_*, query_*, gem_create, ...).
  Fixed: fails=0.
- Laptop trial c2683ee4ddd3 (`#1 irisseg-fix`, main 612d0f90 + fix): greetd.log
  `gpu-env: renderer=iris GL_RENDERER='Mesa Intel(R) UHD Graphics 620 (KBL GT2)'`, `iris GLES 3.2 wgpu=1`,
  `profile: session starting ... renderer=iris`; cosmic-comp + cosmic-greeter-login running;
  klog panic/SMAP/WDOG lines = 0; wifi up (ssh). PROMOTED: good = c2683ee4ddd3.

## Tests

- aarch64 regress (fixed, release): overall PASS, boots failed 0/1.
- aarch64 `--suite i915fake`: i915test 1/1 RC=0. x86_64 `--suite i915fake`: fails=0 (unfixed: 38).
- x86_64 built (release) and booted on the laptop (above).

## Open

- NVIDIA uapi (drivers/nvidia/src/uapi.rs `read_user`/`write_user`, nested pointers such as
  `infop`) still dereferences user pointers raw; on x86_64 with SMAP on, a nested user pointer access
  there is a SMAP violation (panic). Untested on the PC since smapx86. Convert it to sched::uaccess and
  set `user_args: true`.
- A full user login (password over greetd IPC) was not driven; the greeter session is up on iris.
