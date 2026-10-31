# lane/kmodlower (U2), 2026-10-31

Built on lane/kmodgate (U1). New file kernel/src/kmod/lower.rs; edits in exports/{irq,common,pci,net}.rs, registry.rs, sys.rs.

## Design as built
- IRQ: handlers are wrapped (`lower::wrap_irq`) in a leaked ctx holding a `ModFn<KIrqHandler>` of the registration's own gate (Lower), so a delivery never uses the interrupted code's generation. IrqHandle is kept in `lower::IRQS` (no mem::forget). release_one(IRQ) calls `free_irq`; the Lower drain before exit is the "wait for in-flight handler". rp1 IRQs are still unreleasable (-> Zombie).
- hrtimer: `kapi_hrtimer_new` takes the owner from the running gate, wraps cb in ModFn, records RegKind::HRTIMER (new, 18). release = cancel_sync + drop. No module-visible free export (NEXPORTS unchanged).
- kthread: U1's stop flag + Lower drain; fixture kmbad_thread polls kapi_kthread_should_stop.
- hwrng: slots are a bitmap (HW_USED cleared on unregister), read fn is a gated `Arc<ModFn>`, name is an owned per-slot leaked copy (reused when equal), `sched::random::unregister_source` added.
- PCI quiesce (D3.3): bdfs touched via kapi_pci_enable / kapi_pci_cfg_write / irq requests are recorded per generation; before the lower drain the kernel clears MSI/MSI-X enable and bus master and sets INTx-disable.
- DMA ledger: kapi_page_alloc/free under a gate are tracked; pages left after exit are logged ("exit left N DMA pages owned: logged, not freed") and never freed.
- netdev: ModNetif calls go through the Upper gate (closed => send/recv/link fail, mac cached). `unbind_upper` (before Upper drain) records the netif; the Netif table entry stays (name kept). Next load with same kind/prefix/MAC rebinds the same netif id and slot, link_gen offset so the daemon sees a new link.
- virtio_rng / virtio_net: `exit:` added; drivers got `shutdown()` (device reset confirmed by status read-back, then free buffer pages; ring pages stay owned and are logged).
- Fixtures kmbad_thread (slot 314), kmbad_freectx (315, frees timer ctx in exit without cancelling).

## Test notes
- Module dirs accumulate under /lib/modules/<build-id>; unloadtest.sh now uses `ls -dt` to pick the newest (a stale dir gave BuildId refusals).
- The virtio_net loop needs virtio_net as =m only (otherwise the built-in owns eth0 and the module becomes eth1): put `CONFIG_MODULES=y` and `CONFIG_VIRTIO_NET=m` in an untracked `.config`. Default build is unchanged.
- Build: KMOD_TEST=1 KMOD_EXTRA="hello kmbad_hang kmbad_reload kmbad_thread kmbad_freectx virtio_rng virtio_net" ./scripts/build-all.sh --arch aarch64

## Not done
- x86_64 not run; MSI-X path (x86) release untested. rp1 IRQ, wait/sem handles, MMIO mappings, Wi-Fi/HCI still have no free.
