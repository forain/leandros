# M2c — Venus/Vulkan investigation on the Linux box, 2026-08-06

Ran on `forain@172.16.158.150`: EndeavourOS, virglrenderer 1.3.0, QEMU 11.0.1,
AMD Ryzen 9 7950X (RADV RAPHAEL_MENDOCINO). Guest: LeandrOS commit `245615b`
on the `aarch64-kernel-softfloat` branch (aarch64 kernel built softfloat, per
the M7z4/M2 FP-SIMD fix). QEMU device line for all runs:

```
-device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G -display egl-headless
```

These files were pulled off the Linux box's `/tmp` before a reboot could
clear them; the box's `/tmp` originals were left in place, untouched.

## Results

**x86_64 / KVM** (`venuswave.py` → `venuswave_x86_64_serial.log`):
venustest 68/68, vktest 2/2 runs with 0 failures, drmsmoke 22/22,
vfstest 36/36. Clean.

**aarch64 / TCG** (`venuswave.py` → `venuswave_aarch64_serial.log`,
re-run via `venuswave2.py` → `venuswave2_aarch64_serial.log`):
venustest 68/68, drmsmoke 22/22, vfstest 36/36. Clean — the softfloat
kernel fix holds under this workload too.

## Open finding: vktest hangs at vkEnumeratePhysicalDevices under TCG

`vkhang.py` (aarch64/TCG) → `vkhang_serial.log` + `vkhang_regs.txt`:
vktest hangs inside `vkEnumeratePhysicalDevices`. `vkhang_x86tcg.py`
(x86_64, but forced `-accel tcg` instead of KVM) → `vkhang_x86tcg_serial.log`
reproduces the identical hang. Since x86_64/KVM passes cleanly (see above)
but x86_64/TCG hangs the same way as aarch64/TCG, this is a **TCG timing
issue, not an arch- or softfloat-related bug**. The guest stays healthy
during the hang (not a crash or panic), all vCPUs go idle, and Ctrl-C
recovers cleanly — `vkhang_regs.txt` captures vCPU register state at the
point of the hang via the QEMU monitor for follow-up. Prime suspect is the
polled, ISR-less GPU completion path (no device-IRQ infra yet — this
project's kernel still polls even the keyboard), which is presumably far
more timing-sensitive under TCG's software-emulated instruction pacing than
under KVM's near-native execution.

## Hang duration

The aarch64 `vktest` hang at `vkEnumeratePhysicalDevices` was left running for
**2402 s (~40 min)** with zero new serial output before being killed with `pkill`
(`### vktest_run2 (2402s) completed=False`). The `Traceback` / `BrokenPipeError`
at the tail of that run is the harness losing its QEMU to that kill, not a guest
fault. 40 minutes of silence rules out "slow under TCG" — the thread is blocked
waiting for a completion that never arrives.
