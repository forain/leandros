# lane/kmodunloadtest (U4), 2026-10-31

- ktest=kmodwin is relative: present_pages compared against the value at test start, and the test slot steps down from TEST_SLOT_OFF to the first free slot (KMOD_TEST autoloads kmodtest/kmodbad into the two top slots, so the old fixed slot failed "reserve test slot" and "map RW" there).
- kmodctltest in the KMOD_TEST image: two causes of the SKIP. kmod-build.sh truncated modules.load (dropping kmodtest), and mkfs-f2fs-populated.py staged only target/kmod/<arch>/rootfs when it existed (kmodtest lives in initrd/). Now kmod-build keeps kmodtest in modules.load under KMOD_TEST and mkfs merges both trees.
- Wiring: autoloadtest is a standalone host script, in no regress list; unloadtest/e1000etest follow it. New scripts/kmod/kmodregress.sh <arch> [unload|e1000e|autoload|ctl] builds with KMOD_TEST=1 and runs them; the default all-Y regress is untouched and not slowed.
- e1000e DMA leak: drivers/src/e1000e.rs free_rings() (module-only) frees descriptor and buffer pages through the kapi page free after shutdown(); module exit() calls it and logs the count. e1000etest.sh now fails on any "exit left N DMA pages owned".
- Docs: "Unloading modules" section in docs/kernel/modules.md; the old "modules never unload" text fixed.
- Env notes: build needs LEANDROS_SKIP_E2FSPROGS=1 on this Mac (no container tool). Untracked .config used, not committed.

## Results
- aarch64 (KMOD_TEST, =m drivers): unloadtest UPPER=1 all ok; kmodctltest ALL PASS (was SKIP); ktest=kmodwin PASS; autoloadtest virtio_rng PASS (run WITHOUT KMOD_TEST: kmodtest in modules.load defeats its isolation check, so kmodregress.sh's autoload step needs a separate non-KMOD_TEST build).
- x86_64: e1000etest 7/7 ok, 10 x "freed 258 DMA pages", 0 "DMA pages owned"; unloadtest UPPER=1 35/35 ok.
- Not done: laptop trial (orchestrator).
