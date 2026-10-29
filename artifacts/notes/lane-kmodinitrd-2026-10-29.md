# lane/kmodinitrd 2026-10-29

Laptop symptom: shipped initrd carried only the direct kernel's lib/modules/<build-id>, while deploy-kernel ships target/final-x86_64/kernel (Limine variant, other build-id).
Fresh builds stage BOTH variants (kmod-build.sh) and the initrd of a fresh build-all holds both (verified INTEL=1 CONFIG_E1000E=m); the mismatch came from a stale/foreign initrd paired with a different kernel.

Fix: scripts/kmod/bidcheck.py
- `bidcheck.py build <arch>` (hooked into build-all after create_initrd): when any =m / KMOD_EXTRA / KMOD_TEST is set, every final-<arch>/kernel{,-direct} build-id must have lib/modules/<id> in initrd-<arch>.cpio and in target/kmod/<arch>/rootfs (what mkfs packs). Fails the build otherwise. No-op for the default build.
- `bidcheck.py pair <kernel> <initrd>`: used by remote.py deploy-kernel and pi-deploy; refuses an initrd that carries lib/modules but not the uploaded kernel's build-id.
- run-leandros driver: direct x86_64 boot honours LEANDROS_NIC (was -net none) so the e1000e autoload can be tested on the direct kernel.
