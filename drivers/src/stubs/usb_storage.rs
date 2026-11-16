//! `CONFIG_USB_STORAGE=m` (or `=n`): the module owns the disks (modules/usb-storage); these are the
//! entry points other code calls. The kernel keeps the USB hot-plug scan running: it drives the class
//! drivers' detach callbacks and frees the slots of departed devices.
use alloc::string::String;
use alloc::vec::Vec;

pub struct Inflight { pub disk: usize, pub op: &'static str, pub lba: u64, pub bytes: u32, pub age_ns: u64, pub pid: u32 }
pub fn set_cmdline(_cmdline: &str) {}
pub fn stall_dump_tick() {}
pub fn inflight(_now: u64) -> Vec<Inflight> { Vec::new() }
pub fn lock_owners() -> Vec<(usize, u32, usize)> { Vec::new() }
pub fn task_disk_wait_ns(_pid: u32) -> u64 { 0 }
pub fn stats_report(_window_ns: u64) {}
pub fn pm_resume_check() -> Result<String, String> { Ok(String::from("0 usb disk(s) (module)")) }

fn hotplug_thread() {
    loop {
        crate::kenv::sleep_until(sched::monotonic_ns() + 250_000_000);
        for ev in usb::hotplug_scan() {
            // Mass-storage events cannot occur: the enumerator leaves bulk-only interfaces to the class registry.
            if let usb::HotEvent::Gone { bus, slot } = ev { usb::release_slot(bus, slot); }
        }
    }
}

pub fn init() -> usize {
    if usb::controller_count() > 0 && sched::kthread::spawn("usb-hotplug", hotplug_thread).is_none() {
        crate::pci::serial_debug("[USB] could not start the hot-plug thread\n");
    }
    0
}
