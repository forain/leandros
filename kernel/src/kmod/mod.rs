//! Loadable kernel modules: boot-time state (build-id, module window).

pub mod autoload;
pub mod buildid;
pub mod exports;
pub mod gate;
pub mod load;
pub mod lower;
pub mod registry;
pub mod sys;

pub use load::{load_image, KmodErr, Source};
pub use registry::{for_each, note_registration, registrations, ModInfo, State};

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
    // kmod=0 must hold for every autoload route (the USB hook runs before autoload::boot).
    if autoload::opt("kmod") == Some("0") { DISABLED.store(true, Ordering::Relaxed); }
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

/// True when modules cannot be loaded (window init failed, or kmod disabled).
pub fn disabled() -> bool { !window_ok() }

/// The NUL-padded module name as a str.
pub fn name_str(n: &[u8; 32]) -> &str {
    let l = n.iter().position(|&b| b == 0).unwrap_or(32);
    core::str::from_utf8(&n[..l]).unwrap_or("?")
}

fn log(s: &str) { drivers::pci::serial_debug(s); }
