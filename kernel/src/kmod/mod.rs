//! Loadable kernel modules: boot-time state (build-id, module window).

pub mod buildid;

use core::sync::atomic::{AtomicBool, Ordering};

static WINDOW_OK: AtomicBool = AtomicBool::new(false);
static DISABLED: AtomicBool = AtomicBool::new(false);

/// Disable kmod (a prerequisite failed). Boot continues without modules.
pub fn disable() { DISABLED.store(true, Ordering::Relaxed); }

/// True when the module window is initialised and kmod is not disabled.
pub fn window_ok() -> bool { WINDOW_OK.load(Ordering::Relaxed) && !DISABLED.load(Ordering::Relaxed) }

/// Called once after `mm::kva::init()`.
pub fn init() {
    log(&alloc::format!("[kmod] build-id {}\n", buildid::build_id_hex()));
    if DISABLED.load(Ordering::Relaxed) {
        log("[kmod] window disabled\n");
        return;
    }
    if mm::kmodwin::init() {
        WINDOW_OK.store(true, Ordering::Relaxed);
        log(&alloc::format!("[kmod] window {:#x}+{}M ok\n", mm::kmodwin::BASE, mm::kmodwin::SIZE >> 20));
    } else {
        DISABLED.store(true, Ordering::Relaxed);
        log(&alloc::format!("[kmod] window {:#x} BUSY {} pages, kmod disabled\n", mm::kmodwin::BASE, mm::kmodwin::present_pages()));
    }
}

fn log(s: &str) { drivers::pci::serial_debug(s); }
