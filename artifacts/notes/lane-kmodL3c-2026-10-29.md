# lane kmodL3c (2026-10-29): module loader, registry, core kapi

DELIVERED. Branch lane/kmodL3c.

- kernel/src/kmod/{load,registry,exports}.rs; `load_image(img, Source)`; segments are written through the HHDM alias of buddy frames, then mapped once with the final permission (R / RX kernel-only-X / RW). No page is ever W+X; aarch64 RX and RW both carry UXN.
- aarch64: `dc cvau` per DminLine over each RX page's HHDM alias, `dsb ish; ic ialluis; dsb ish; isb`, then `sched::trigger_preempt` (resched IPI) to every other active CPU. x86 needs nothing.
- Review item 3: init < 0 with no registrations unmaps, frees the frames, `release()`s the slot and drops the registry entry (retry works). With registrations the pages leak and the entry is `Failed`. init >= 0 is Live (the test blob returns 42+owner).
- Registry: id counter from 1, owner-tagged registrations (`note_registration`, `registrations`, `for_each`), `kmod::disabled()`.
- 14 kapi exports, all `#[no_mangle] extern "C"` and in `.ksymtab` (224 bytes): kapi_abi_version, kapi_log, kapi_panic, kapi_alloc, kapi_dealloc, kapi_realloc, kapi_cmdline, kapi_monotonic_ns, kapi_msleep, kapi_udelay, kapi_spin_held, kapi_spin_held_count, kapi_kthread_spawn, kapi_ktest_ping. `kmod_abi::sys` now has the fn-type aliases and the extern block; exports.rs checks every signature against them by const coercion, and asserts the window constants equal kmod_abi::layout.
- `sched::spinheld::account` added (hook body).
- ktest=kmodload (opt-in, EXTRA): synthesises blobs at run time; checks positive load (init ran once, ctx owner/size 48/abi 1, registry Live, rodata readback), W^X and UXN over every present window page, CPU1 executes the new code (pinned kthread), spin hook and owner-tagged kthread via the table, 7 negatives that leave `present_pages()` unchanged, and a failing init (-5) that frees the slot and reloads twice.
- Evidence: PASS aarch64 Limine (-smp 4 TCG), x86_64 Limine, x86_64 direct `-kernel`. `llvm-nm kernel | grep -c ' [tT] kapi_'` = 14 on aarch64 kernel; ksymtab length 224 B = 14 entries.

Deviations: kapi_log has no owner arg, so it prints "[kmod] <level> msg" (no module name). Ktest slots: TEST_SLOT_OFF and the next 256 KiB slot.
Not run: rpi5 kernel nm count; aarch64 direct ktest (no cmdline on aarch64 direct).
