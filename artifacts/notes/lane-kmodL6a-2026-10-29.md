# lane kmodL6a (2026-10-29): e1000e-adapt, Y-only refactor + kapi exports

DELIVERED. Branch lane/kmodL6a.

- ksymtab 22 -> 33. New kapi: pci_enable, pci_find_cap, irq_request_msi, irq_request_line, wait_new/timeout/notify, netdev_register, netdev_kick, netdev_kick_from_irq, pm_register. KMAP_UC added to kapi_map_device (kva::ioremap Uncached, the mode e1000e's map_bar uses).
- kmod-abi: KMAP_UC, KNET_*, KNetifOps (10 extern "C" ops), KPmFn, KIrqHandler, sys decls. kmod-kapi shim: pci::{enable,find_capability,cap}, mm::kva::{CacheMode,ioremap}, irq::{request_msi,request_line}, netdev::{LinkMon (handle over wait_new),register,kick,kick_from_irq}, pm::register.
- Kernel: `netdev::register_late` (serialised, kicks netd; the daemon's per-pass loop builds the stack for a netif that appears later, as it already did for wifi0). Module netifs are adapted by a static `MOD_OPS` keyed by slot index. pm hooks: `kmod::exports::{pm_suspend,pm_resume,pm_shutdown}` return Some only when a module registered; pm.rs and power.rs try them first and fall back to the builtin e1000e.
- Seam: drivers/src/kenv.rs now also exports `irq` and `netdev`; e1000e.rs takes `irq, mm, netdev, pci, sched, PciFunction` from kenv (no `crate::irq/netdev/pci` left).
- Deviations from the outline: irq_request_* and pm_register take `owner` (like kthread_spawn/hwrng); `kapi_wait_timeout(h, ns)` tracks the waiter's "seen" counter inside the handle (one waiter per handle), so the shim's `LinkMon::wait` ignores its `seen` argument; IRQ handler returns u32 0/1 (IrqReturn repr); pm hooks are not tied to a RegKind (no unload yet). PciFunction facade over bdf is left to L6b.
- ktest=kmodkapi extended (netdev late registration visible to netd: "[NET] ktn0: stack up"; pm hooks; wait queue; pci enable/caps; irq errors).

Evidence: x86_64 symbol diff vs main (llvm-nm, hash/.NNN stripped): only the new exports, MOD_OPS/MOD_NETIF/PM_HOOKS/LATE_LOCK/pm_* and ktest items, plus inliner noise (LinkMon::wait now out of line, Netif::info_lines, e1000e_hw Rx/TxRing::init, msi_message, wifi_wrongkey, i915 read_user/drop_in_place). x86_64 with LEANDROS_NIC=e1000e (default NIC is virtio-net): `[e1000e] interrupt: MSI vector 0x43`, DHCP 192.168.105.12, /bin/nettest PASS. ktest kmodload+kmodkapi PASS on x86_64 and aarch64. aarch64 default regress PASS.
