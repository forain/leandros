# lane/kmodbus (W2-2) 2026-10-29 -- of/sdio/pci-class module aliases, platform registry, boot module check

FIXED / DELIVERED
- lib/kmod-abi/src/alias.rs: shared modules.alias parser (pci 3-token + class, usb, of, sdio, malformed) + fnv1a32; BUS_SDIO=5.
- kernel/src/kmod/autoload.rs: one cached alias file, of_hook, sdio_hook, PCI class matching, kmod=0 honoured,
  `plat.alias_selftest=1` selftest, klog::on_disk_registered(). Log format: `[kmod] alias <bus> <id> <mod>: <err>` /
  `[kmod] <bus> <id> -> <mod>`; autoloadtest.sh strings updated to it.
- drivers/src/platform.rs registry; boot DT walker for_each_compatible; init.rs prints `[plat] DT compatibles`.
- kmod-rt `module!` of:/sdio: keys (.kmod.ofstr), kmodpost.py alias emission + OF hash/len check; modules/hello of: leandros,test.
- modules/BOOT (comments only; W2-3 adds entries), kmod-build.sh keeps BOOT order, bidcheck.py `bootcheck`, pair refuses missing .kmod.
- driver.py LEANDROS_ROOT_BUS=virtio|nvme|usb.

GATES (aarch64 unless noted)
- autoloadtest: of PASS, virtio_rng PASS, uvc PASS, e1000e (x86_64) PASS; selftest.sh 0 FAIL.
- aarch64 default regress PASS; box x86_64 regress and kexecfdttest: see merge report.

M6 SCAFFOLD: scripts/kmod/kexecblktest.sh (prints SKIP). Real gate lands in W2-3 once CONFIG_VIRTIO_BLK=m builds:
build =m with root on virtio-blk + BOOT entry, kexec into same kernel (kexecfdttest.py flow), assert root mounts.
kapi_plat_register deferred to W2-8 (recorded in decisions file "After W2-2").

OPEN: none in-lane.
