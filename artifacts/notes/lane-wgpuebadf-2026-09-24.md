# Lane wgpuebadf (2026-09-27)

Branch `lane/wgpuebadf`, head `35543f78`, merged into integ-wave-0924 by the orchestrator. Base e15f890a.

## Commits
- `79bcdb7d`: the Mesa stage ships `libleandros_ssp.so.1`, which defines `__stack_chk_guard`.
  - Why: aarch64 GCC reads the stack canary from a global symbol; x86_64 reads it from `%fs:0x28`. LeandrOS `libc.so` does not export that symbol, so Alpine's prebuilt `libvulkan.so.1` failed to relocate on aarch64.
  - Fix: `patchelf --add-needed` puts the shim into `libvulkan.so.1` and `libzstd.so.1`. The build fails if any shipped ELF still imports the guard without the shim. mkfs stages the shim.
- `35543f78`: `gpuprobe gl` also prints `GL_WGPU=1|0`.
  - It creates the context the way wgpu-hal does: robust access first, then a retry without it only on `EGL_BAD_ATTRIBUTE`.
  - `gpu-env`, virgl path: `ICED_BACKEND=wgpu` when `GL_WGPU=1`. Otherwise it uses `wgpu,tiny-skia` and prints a banner.
  - Also adds an EBADF syscall tracer, `SYSCALL_TRACE_EBADF`, off by default. It prints nr, args, image name, and on aarch64 the user pc/lr plus a frame-pointer backtrace.

## Shipped
The patched Mesa (EGL robustness patch 0001), the shim and the new gpuprobe are in `m3-gl-stack/gpu-stage-{aarch64,x86_64}` on the Mac, the desktop and the laptop. The previous stages are kept as `*.prev` (and `*.prev.log`); to revert, move them back.

## EBADF — NOT REPRODUCED (premise not confirmed)
Mac aarch64/HVF, virgl on ANGLE/Metal, patched Mesa. Each boot:
1. greeter up about 50 s;
2. log in as leandro;
3. 8 rounds of Super+W ×2, then Super, type "te", Esc.

The tally counts "Bad file" lines in greetd.log plus the serial `[EBADF]` tracer.

| tree | boots | client EBADF | greeter restarts | notes |
|---|---|---|---|---|
| current (e15f890a + lane) | 5 | 0 | 0 | every client: `virgl (ANGLE Metal…)`, `backend: Gl`; tracer: 5 hits in one boot, all cosmic-comp `readlinkat(/proc/self/fd/N)` |
| old base 338a65c2 (wgpuapps head) + shim | 4 | 0 | 0 | also clean, so this is NOT evidence that the later merges fixed it |

wgpuapps' failing runs used a scratch build whose exact kernel and stage are unknown. It was deleted in the Mac disk cleanup. The EBADF could not be reproduced on either tree, so it is still unexplained. It may depend on something the scripted loop does not exercise, such as a real launcher daemon restart. The tracer is ready to be turned on if it comes back.

x86_64 virgl on the desktop (KVM, radeonsi):
- boot 1: greeter + session, 0 EBADF, clients on `Gl`.
- boot 2: QEMU exited at the host with `amdgpu: The CS has cancelled because the context is lost` right after login. The boot script then died with a broken pipe.
- This is a host GPU reset and has nothing to do with EBADF. It still needs watching on radeonsi virgl with wgpu clients.

## Not done (moved to the integration verifier)
15-minute session, greeter latency, drmsmoke, Venus sanity check.
