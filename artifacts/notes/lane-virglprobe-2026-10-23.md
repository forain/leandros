# lane/virglprobe 2026-10-23 (L11.13 virgl probe SIGSEGV) -- FIXED by 618c6cc5, guard added

Root cause: strict PAN (panfix2 45bd2ea6) made the remaining raw user accesses in
drivers/src/drm_device_interface.rs fatal (`[PAN] raw kernel access to user memory`
-> process killed, RC=139). Sites hit by Mesa's virgl winsys: RESOURCE_CREATE, GET_CAPS
(nested caps pointer), atomic commit after ADDFB2. lane/uaccess2 (618c6cc5) bounces
those ioctl args through uaccess copies; no further change needed here.

Guard: runtests `drm` suite now runs `/bin/gpuprobe gl` after drmsmoke (gpu-env's
exact COSMIC gate; RC=0 only for a hardware renderer).

Evidence (aarch64 HVF, `runtests.py aarch64 <tag> --suite drm --virgl`):
- red, 5847c112 (pre-uaccess2) + guard: drmsmoke RC=139, gpuprobe gl RC=139, 3 [PAN] lines.
- green, 618c6cc5 + guard: drmsmoke RC=0, gpuprobe gl RC=0
  (GL_RENDERER='virgl (ANGLE (Apple, Vulkan 1.1.357 (Apple M4 Max ...'), 0 [PAN] lines.
- `driver.py start aarch64 --virgl`: cosmic-greeter renders the login card at 90 s, 0 [PAN].
