# lane/kexecargs (2026-10-19)

## kexec --append dedup
`userland/kexec/kexec.c`: each `--append` word replaces earlier cmdline words
with the same key (text before the first `=`, whole word for flags); order
otherwise kept. /proc/cmdline is never printed (it can hold Wi-Fi PSKs).
Test (aarch64 uefi-tcg): `kexec /boot/kexec --initrd=/boot/kexec-initrd
--append=kexec.gen=1`, then `--reuse-cmdline --append=kexec.gen=2`:
`[early] cmdline:` was `kexec.gen=1`, then `kexec.gen=2` (not `gen=1 gen=2`).

## --direct had no usable root (real bug, both arches)
Cause: the default `/dev/root` is vdb (vda is the Limine disk in the UEFI
layout). Direct boot has no Limine disk, so the root image is vda and the
kernel mounted the 8 GiB home volume (vdb) as root: no /bin/login.
- x86_64: multiboot passes `-append` as the cmdline (boot/src/multiboot2.rs
  already parses it); `-append root=/dev/vda` added. Reaches login, root works,
  `/dev/root -> vda`.
- aarch64: a bare ELF gets no cmdline, so instead a 1 MiB `null-co` virtio
  disk is added first as a stand-in vda; root stays vdb. Reaches login.
Changed in `scripts/run-qemu.sh` and `.claude/skills/run-leandros/driver.py`.
