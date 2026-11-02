# lane/kmodexit 2026-11-01

What: exit() for virtio_keyboard, virtio_snd and hda so all three unload and reload.
- Drivers free their DMA (hda free_dma + capture ring, snd free_dma, virtio_keyboard free_all); phys = virt - HHDM offset (mm::virt_to_phys is a pure offset, verified).
- input_release clears the fixed-node claim; knob kmod.leakinput=1 keeps the old irrevocable claim for the kmbad_leak Zombie fixture.
- SND backend slot released on unload; busy_reason "sound open" while /dev/pipewire is open (pipewire detach_module/module_busy).
- unloadtest EXIT3=1 (run ul4): x20 reload per module, EBUSY with `sleep 6 > /dev/pipewire &`, hda via intel-hda, virtio_snd re-attaches after hda.

Results (aarch64 QEMU TCG, UPPER=1 EXIT3=1): 52 ok, 0 FAIL, including the default and UPPER sections.

Caveats: aarch64 only; x86_64 not run. The ul3 fixture run blacklists virtio_keyboard because with leakinput=1 it would itself zombie and hold the pointer claim. hda was not tried on real hardware.

## Review fixes
- Sound race: sys_delete_module, after the Upper drain, calls registry::seal_sound -> pipewire seal_module (sets SND_GOING, then re-checks module_busy). Open paths (writer, nonblocking, claim) test SND_GOING after claiming and fail -EBUSY (Dekker handshake: either the open sees the flag or seal sees the open). Refusal backs out like busy_reason (unseal, reopen Upper, Live, EBUSY); zombie() unseals; detach_module clears the flag.
- input_release: tracks pressed codes of fixed nodes 0/1, pushes key-up for each plus SYN before dropping the claim.
- virtio status reset read-back (snd, virtio_keyboard); hda RUN-stuck log; hda free_dma zeroes capture ring/bdl; stale comments fixed.
- NOT done: virtio_keyboard BAR/common_cfg unmap (module-side kenv::mm has no unmap for map_kernel_device; one small VA leak per reload; comment left in free_all).
- Tests: EXIT3 race loop (12 rounds, open /dev/pipewire as rmmod starts; RACEBAD if a held fd coexists with rmmod rc 0). Held-key test skipped (no sendkey channel in runner).
- Results: UPPER=1 EXIT3=1 aarch64: 54 ok, 0 FAIL; check-rpi5 passes.
