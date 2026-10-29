# lane kmodL6b (2026-10-29): e1000e as a loadable module

DELIVERED. Branch lane/kmodL6b.

- `modules/e1000e` (`kmod-e1000e`): `#[path]`-includes `drivers/src/e1000e.rs` and `e1000e_hw.rs` unchanged apart from the log seam. `kenv` = `kmod_kapi::{irq, mm, netdev, pci, sched, serial_raw}`. `module!` lists all 57 ids of `hw::DEVICES` (8086:10d3 QEMU 82574L, every I219); a const assert ties the list length to the table. `init()` runs the driver init (cmdline via `kapi_cmdline`), then `netdev::register(Wired, "eth", &OPS)` and `pm::register(suspend, resume, shutdown)`; -19 when no NIC.
- kmod-kapi additions: `pci::PciFunction` facade over a bdf (read/write 8/16/32, find_capability, enable_mem_and_bus_master, bar, map_bar with BAR sizing, intx_line), `PciDevice::function`, `mm::buddy::alloc_below`, `sched::kthread::{spawn, udelay, msleep}`, `irq::{IrqSource, request_irq}` (INTx; resolves the bdf remembered by `intx_line`), `request_msi/line` accept `impl Into<u32>`, `LinkMon::line`, `cmdline()`, `serial_raw()`. No kapi/ABI change.
- `drivers/src/kenv.rs` gets `serial_raw` (Y: `serial_print_str_raw`); e1000e.rs `log()` goes through it, so the module's lines appear as `[kmod e1000e] <6> [e1000e] ...`.
- `drivers/Kconfig`: `E1000E` is `tristate`, default y. Defconfigs unchanged (features md5 identical to main for x86_64, x86_64_laptop, aarch64, rpi5).
- `scripts/kmod/autoloadtest.sh [arch] [virtio_rng|e1000e]`: the e1000e variant builds MODULES=y E1000E=m, boots with LEANDROS_NIC=e1000e and checks autoload, probe line, MSI, DHCP, nettest, /proc/modules, kmodctl list, and the modules.blacklist=e1000e boot (no eth0 DHCP).

## Evidence
- x86_64 (TCG, LEANDROS_NIC=e1000e): `[kmod] e1000e loaded id 1`, `[e1000e] interrupt: MSI vector 0x43`, `eth0: link up 1000 Mbps`, DHCP 192.168.105.x, nettest 21 passed / 0 failed / 2 skipped, `e1000e 4194304 3 - Live`, `kmodctl list` shows it. autoloadtest.sh x86_64 e1000e: PASS.
- `modules.blacklist=e1000e`: `alias 8086:10d3 e1000e: Blacklisted`, boots, no eth0.
- x86_64 `--mode direct`: boots with module infra; the direct launcher has no NIC (pre-existing), so nothing matches the alias and nothing loads.
- aarch64 (LEANDROS_NIC=e1000e works in the harness): module loads, no MSI so `interrupt: INTx line 35` through the shim's request_irq, DHCP, nettest 21/0/2.
- All kmodpost guards and kmodcheck pass on both arches.
- Sizes: x86_64 module 107,968 B, kernel =m 6,818,952 vs =y 6,886,592 (-67,640). aarch64 module 120,904 B, kernel =m 6,945,192 vs =y 7,019,528 (-74,336). Slot 4 MiB reserved.
- Laptop-flavoured kernel (INTEL=1, E1000E=m) staged at scratchpad `l6b-laptop/` (see report).

## Notes
- `map_bar` sizes the BAR without masking local IRQs (no kapi for it); fine in init, before anything else touches the device.
- `intx_line` is only meaningful on the aarch64 QEMU virt swizzle; x86 uses MSI.
- E1000E=m for module testing; defconfigs keep y. Not tested: unload (no unload in v1), real-hardware suspend/resume through the registered hooks.
