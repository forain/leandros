# Lane hostgpufault — 2026-09-27

Branch `lane/hostgpufault` from `origin/integ-wave-0924` @ 0c8d50a, head `7960651`, pushed.
Worktrees: desktop `/run/media/forain/samsung970pro512/leandros-siblings/leandros-hostgpufault`
(the Mac `~/code/leandros-hostgpufault` was only used for an aarch64 launch smoke test).
Desktop scratch: `/run/media/forain/samsung970pro512/hostgpufault-tmp/` (loop logs, `boots/<tag>/` serial + QEMU stderr, apitrace build).

## Outcome: host driver bug (radeonsi threaded context). Our kernel is not the cause. Worked around in our launchers.

Commits:
- `b846ec4`, `c621497`: `.claude/skills/run-leandros/gpu-session-loop.py` soak tool, plus a SKILL.md note.
- `7960651`: `run-qemu.sh` and `driver.py` export `GALLIUM_THREAD=0` to QEMU when all of these hold: the host is Linux, a host render node's driver is amdgpu, a GL display (virgl or Venus) is in use, and the caller has not set `GALLIUM_THREAD` itself.

## Evidence

Fault signature, the same every time:
- `[gfxhub] page fault ... Faulty UTCL2 client ID: SQC (data)`, `PERMISSION_FAULTS 0x3`, read. That is a shader scalar load: a descriptor or constant fetch.
- Addresses: 0x0, **0x3f800000** (the bit pattern of the float 1.0), 0x80010ee000, 0x8001017000, 0x800001f78000. These are garbage values used as pointers.
- Then `ring gfx_0.0.0 timeout`, a ring reset, and QEMU exits with `amdgpu: The CS has cancelled because the context is lost`.
- Process is `qemu-system-x86:cs0`, radeonsi's submit thread.

Why the guest cannot cause it:
- Without `VIRTIO_GPU_F_RESOURCE_BLOB`, which the host does not offer in virgl mode, the host GPU never maps guest pages. Transfers are CPU copies.
- The GPU virtual address comes from the host driver.

The suspects from the lane brief (resource-id reuse, backing pages freed early, TRANSFER stride) are therefore **REFUTED** as causes. Two more points against them:
- The kernel serial logs show no EXECBUFFER, ctrlq-timeout or unknown-bo errors.
- The same signature, including `STATUS 0x00501430`, is reported from bare-metal Chromium/Electron on the same Raphael iGPU (anthropics/claude-code#82661, Mesa 26.0.3).

Reproducer: x86_64/KVM, `--virgl`, COSMIC session, open cosmic-term (Super+T), type `ls -la /bin`, open the app library, then Super+Q; 6 rounds (`APPS=1 gpu-session-loop.py`). Host: Mesa 26.1.3 radeonsi, virglrenderer 1.3.0, QEMU 11.0.1, kernel 7.0.13.

| config | boots | boots with host fault |
|---|---|---|
| virgl, session toggles only (no app launch) | 10 | 0 |
| virgl + cosmic-term, default | 2 | 2 |
| same with AMD_DEBUG=nooptvariant / nodmashaders / nonggc | 2 / 1 / 2 | 1 / 1 / 2 |
| same under apitrace (timing perturbed) / GALLIUM_DDEBUG | 3 / 1 | 0 / 0 |
| Venus (RADV) + cosmic-term | 6 (+6 without apps) | 0 |
| **virgl + cosmic-term, GALLIUM_THREAD=0** (manual 6 + via launcher 16) | **22** | **0** |

- QEMU CPU per scripted boot with GALLIUM_THREAD=0 was 92–127 s (utime+stime). There is no clean baseline with threading on, because those boots die.
- The terminal was confirmed to open with GALLIUM_THREAD=0 (screenshot strip).
- aarch64: this is host-launcher only. A Mac aarch64/HVF `--virgl` boot with the new `driver.py` reached a shell, and the variable is correctly not set (no amdgpu).

## Upstream report material (not filed)
- The race: radeonsi u_threaded_context in QEMU's shared-EGL-context virglrenderer process.
- A deterministic replay did not work: under apitrace the fault never fired (0/3), so it is timing-dependent.
- `AMD_DEBUG=check_vm` needs dmesg access, and `kernel.dmesg_restrict=1` on the desktop with no sudo.

## Side findings (not fixed, guest side)
- virglrenderer logged `vrend_set_single_sampler_view: Illegal handle N` → `failed to dispatch DRAW_VBO` in ctx 1 (cosmic-comp) and in client contexts in 5 boots. It is not correlated with the faults. The guest Mesa virgl stream references a sampler-view handle that does not exist.
- One boot logged `virtio_gpu_virgl_process_cmd: ctrl 0x103 (SET_SCANOUT), error 0x1203 (invalid resource id)` ×3. The kernel pointed the scanout at a resource the host had already dropped, which suggests a framebuffer outliving its BO's host resource.
- virgl contexts are created with the debug name "leandros-venus".
