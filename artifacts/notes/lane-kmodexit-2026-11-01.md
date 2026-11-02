# lane/kmodexit 2026-11-01

What: exit() for virtio_keyboard, virtio_snd and hda so all three unload and reload.
- Drivers free their DMA (hda free_dma + capture ring, snd free_dma, virtio_keyboard free_all); phys = virt - HHDM offset (mm::virt_to_phys is a pure offset, verified).
- input_release clears the fixed-node claim; knob kmod.leakinput=1 keeps the old irrevocable claim for the kmbad_leak Zombie fixture.
- SND backend slot released on unload; busy_reason "sound open" while /dev/pipewire is open (pipewire detach_module/module_busy).
- unloadtest EXIT3=1 (run ul4): x20 reload per module, EBUSY with `sleep 6 > /dev/pipewire &`, hda via intel-hda, virtio_snd re-attaches after hda.

Results (aarch64 QEMU TCG, UPPER=1 EXIT3=1): 52 ok, 0 FAIL, including the default and UPPER sections.

Caveats: aarch64 only; x86_64 not run. The ul3 fixture run blacklists virtio_keyboard because with leakinput=1 it would itself zombie and hold the pointer claim. hda was not tried on real hardware.
