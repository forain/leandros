# Dual-boot ESP chain (proven in aarch64 QEMU, Limine 11.4.1)

ESP /EFI/leandros/BOOTAA64.EFI   Limine binary copied as-is
ESP /EFI/leandros/limine.conf (read from the EFI binary's own directory):
    timeout: 0
    serial: yes

    /LeandrOS
    	protocol: efi
    	path: guid(<p4 PARTUUID>):/EFI/BOOT/BOOTAA64.EFI
ESP /loader/entries/leandros.conf:
    title LeandrOS
    efi /EFI/leandros/BOOTAA64.EFI
    sort-key zz-leandros
ESP /EFI/leandros/.leandros-owned   marker (p4 PARTUUID)
p4 (FAT, type 0E02ED9A-..., bit 63): EFI/BOOT/BOOTAA64.EFI (inner Limine), limine.conf
    (protocol: limine, path/module_path: boot():/..., cmdline: root=PARTUUID=<root>),
    kernel.elf, initrd.gz, leandros/owner (marker), leandros/k/* slots.
Direct variant also works: outer entry `protocol: limine`, `path: guid(<p4>):/kernel.elf`.
Linux stays the default (no sort-key sorts before zz-leandros); the proof switched
loader.conf `default leandros.conf` on a scratch copy only.
New partitions must be 4 KiB aligned or the kernel's f2fs refuses them.
