# lane kmodL5b (2026-10-29): virtio_rng as the first real module

DELIVERED. Branch lane/kmodL5b.

- `modules/virtio-rng` (`kmod-virtio-rng`, workspace member): `#[path]`-includes `drivers/src/virtio_rng.rs` unchanged apart from a new `is_bound()`; its `kenv` is `pub use kmod_kapi::{mm, pci, sched}`. `module!{ name: "virtio_rng", ids: pci(1af4,1044), pci(1af4,1005) }`; `init()` runs the driver probe, returns 0 if bound else -19. Slot reservation was already in `modules/SLOTS` (4 slots, 1 MiB).
- `drivers/Kconfig`: `VIRTIO_RNG` is `tristate`, default y. With =m the kernel builds the existing `=n` stub; the module registers through `kapi_hwrng_register`, which reseeds the CSPRNG (`[RANDOM] reseeded (late source) from: virtio-rng=32B`).
- `scripts/kmod/autoloadtest.sh [arch]`: committed test of the modules.alias autoload path (builds MODULES=y VIRTIO_RNG=m, checks the autoload line, bound, reseed, /proc/modules, kmodctl list, no modules.load, and the blacklist boot). Restores `.config`. Takes a full build-all.

## Evidence
- All kmodpost guards pass on first build; `kmodcheck: ok 3 segs`. Module 25288 bytes (slot 1 MiB).
- aarch64 Limine, =m: no builtin `[RNG]` line; `[kmod virtio_rng] virtio-rng bound`, `[kmod] virtio_rng loaded id 1`, `/proc/modules`: `virtio_rng 1048576 1 - Live`, `kmodctl list` shows it, `head -c 16 /dev/urandom | wc -c` = 16.
- x86_64 Limine (via autoloadtest.sh PASS) and x86_64 `--direct` boot: same lines.
- `modules.blacklist=virtio_rng`: `alias 1af4:1044 virtio_rng: Blacklisted`, boots clean, no reseed line.
- Size (aarch64 Limine kernel ELF): =m kernel 6,991,928 B vs =y kernel 6,992,952 B (-1 KiB); module 25,288 B, so the module path costs about +24 KiB overall.
- Defconfig `to-cargo.py features` md5 identical to main for x86_64, x86_64_laptop, aarch64, rpi5. aarch64 default (=y) regress PASS.

## Notes
- `VIRTIO_RNG=m` is for module testing: the virtio entropy misses the boot seed (`random::init` runs before autoload) and only reseeds late. Defconfigs stay y.
- `kmod=0` was not re-tested here (covered by L4b).
