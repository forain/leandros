# lane/virtiorng — 2026-10-02

## Status: DONE (resumed and finished 2026-10-02)
- All code is committed: b658f86, a68910b, 79a8be4, e12c241. Full build-all.sh rc=0. All 18 suites RC=0 on both arches.
- Desktop boot with virgl, logged in as leandro at the greeter:
  - x86_64/TCG: greeter, then a COSMIC session (panel and dock over the Orion wallpaper). Boot log: `virtio-rng=64B cpu=RDSEED=64B`.
  - aarch64/HVF: greeter, then a COSMIC session (panel and dock over the Orion wallpaper). Boot log: `virtio-rng=64B cpu=none`.
  - Neither serial log has a PANIC, SEGV or WDOG line.
- Optional, not done: boot aarch64 direct (`-kernel`) to see `dtb-rng-seed=32B` from QEMU's /chosen/rng-seed. That path is unchanged apart from the extra seed capture.

## Design
- `drivers/src/virtio_rng.rs`: a polled PCI virtio driver for device type 4. It is used on both arches, because x86_64 q35 and aarch64 virt both put virtio on PCI. It accepts modern 0x1044 or transitional 0x1005 and negotiates VERSION_1. It keeps one descriptor and one DMA page, with one request in flight. Each wait is bounded at 200 ms. If a read times out, its request is collected by the next read and never reposted. INTx is disabled. The driver registers itself with `sched::random::register_source`.
- `sched/src/random.rs`:
  - Registered sources are read at boot (64 B) and at every reseed (32 B). They add to RDSEED/RDRAND/RNDR, jitter and clocks; they do not replace them.
  - `add_boot_seed` holds boot-environment material until the first key is derived, then erases it.
  - Gathering now runs under its own GATHER lock with the RNG lock released. Reseed uses try_lock, so only one CPU gathers and the others keep using the current key.
  - At boot, one log line lists every source and its byte count. One more line is logged at the first reseed.
- `boot/src/device_tree.rs` copies `/chosen/rng-seed` and `kaslr-seed` (up to 64 B). This covers QEMU virt direct boot and the Raspberry Pi firmware.
- `kernel/src/main.rs` runs `virtio_rng::init()`, then the DTB seed hand-off, then `random::init()`. All three run before init_task, so before userspace.
- EFI_RNG_PROTOCOL is not used. It is a boot service, and Limine has already exited boot services before the kernel runs.
- Launchers: `-device virtio-rng-pci,disable-legacy=on` was added to every PCI launch in run-qemu.sh and driver.py (UEFI and direct). It is placed last so no existing device changes slot. raspi4b has no PCI, so nothing was added there. `scripts/mac-qemu-gpu` is a QEMU build dir and the deploy scripts target real hardware, so neither needed a change.

## Entropy sources as logged
- aarch64 HVF (UEFI): `virtio-rng=64B cpu=none jitter=4096 samples/33 distinct`. First reseed: `virtio-rng=32B cpu=none`. There is no DTB seed: edk2 hands over ACPI.
- x86_64 TCG (UEFI): `virtio-rng=64B cpu=RDSEED=64B jitter=4096 samples/9 distinct`. First reseed: `virtio-rng=32B cpu=RDSEED=32B`.
- Hardware without the device logs `[RNG] no virtio-rng device`, and the CPU and jitter sources work as before.

## Tests (LEANDROS_QEMU_MEM=4G, runtests.py, PASS/FAIL)
All suites RC=0 on both arches. Counts are aarch64/HVF first, then x86_64/TCG where the two differ:

| suite | PASS/FAIL |
|---|---|
| sigtest | 24/0 |
| sigtest2 | 13/0 |
| timertest | 28/0 |
| pthreadtest | 8/0 (aarch64), 9/0 (x86_64) |
| polltest | 18/0 |
| epolltest | 11/0 |
| wakepolltest | 17/0 |
| smpwaketest | 9/0 |
| jobtest | 8/0 |
| memtest | 27/0 |
| forktest | 8/0 |
| exectest | 29/0 |
| killmt | 11/0 |
| drmsmoke | 78/0 |
| uptrtest | 112/0 |
| vfstest | 59/0 |
| scmtest | 47/0 |
| ptytest | 15/0 |

pthreadtest `getrandom_distinct` and `getrandom_quality` both PASS. chi2(1 MiB) was 296 on aarch64 and 259 on x86_64.

## Host note
The Mac disk filled up (120 MB free) during the first build-all. It failed in the coreutils x86 build (ENOSPC in the shared /Users/forain/code/coreutils target) and again while copying data1. To get the build through, both data1 images were replaced by APFS clones (`cp -c`) and the second build ran with a local `cp -c` tweak in build-all. That tweak was reverted and is not committed. Worth adopting: data1 is a byte copy of data0, and the full copy costs 3.4 GB per arch.
