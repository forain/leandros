//! Boot-time module policy: `kmod=0`, `modules.load=`, `modules.blacklist=`, and the
//! initrd lookup `lib/modules/<build-id>/<name>.kmod`.

use alloc::string::String;
use super::load::{load_image, KmodErr, Source};
use kmod_abi::KmodId;

fn log(s: &str) { drivers::pci::serial_debug(s); }

fn cmdline() -> &'static str { crate::boot_cmdline().unwrap_or("") }

/// The value of `key=` on the command line (last occurrence wins).
fn opt(key: &str) -> Option<&'static str> {
    let mut v = None;
    for tok in cmdline().split_ascii_whitespace() {
        if let Some(r) = tok.strip_prefix(key) {
            if let Some(r) = r.strip_prefix('=') { v = Some(r); }
        }
    }
    v
}

fn list_has(key: &str, name: &str) -> bool {
    opt(key).map_or(false, |l| l.split(',').any(|n| n == name))
}

pub fn blacklisted(name: &str) -> bool { list_has("modules.blacklist", name) }

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 31 && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Load `<name>.kmod` from this kernel's build-id directory in the initrd.
pub fn load_by_name(name: &str, src: Source) -> Result<KmodId, KmodErr> {
    if super::disabled() { return Err(KmodErr::Disabled); }
    if blacklisted(name) {
        log(&alloc::format!("[kmod] {} Blacklisted\n", name));
        return Err(KmodErr::Blacklisted);
    }
    if !valid_name(name) { return Err(KmodErr::NotFound); }
    let path: String = alloc::format!("lib/modules/{}/{}.kmod", super::buildid::build_id_hex(), name);
    let bi = crate::BOOT_INFO_PTR.load(core::sync::atomic::Ordering::SeqCst);
    if bi == 0 { return Err(KmodErr::NotFound); }
    let bi = unsafe { &*(bi as *const boot::BootInfo) };
    match crate::init::extract_binary_from_initrd(&path, bi) {
        Some(img) => load_image(img, src),
        None => {
            log(&alloc::format!("[kmod] {} not found in initrd ({})\n", name, path));
            Err(KmodErr::NotFound)
        }
    }
}

/// Called once from `init.rs`, after `netdev::init` and before userspace starts.
pub fn boot() {
    if opt("kmod") == Some("0") {
        super::disable();
        log("[kmod] disabled by kmod=0\n");
        return;
    }
    if super::disabled() { return; }
    // modules.load= wins; without it, the initrd's lib/modules/<build-id>/modules.load
    // (names separated by commas or whitespace) is the default list. Direct aarch64
    // boots receive no command line, so this is their only route.
    let file: Option<String> = if opt("modules.load").is_some() { None } else {
        let bi = crate::BOOT_INFO_PTR.load(core::sync::atomic::Ordering::SeqCst);
        if bi == 0 { None } else {
            let bi = unsafe { &*(bi as *const boot::BootInfo) };
            let path = alloc::format!("lib/modules/{}/modules.load", super::buildid::build_id_hex());
            crate::init::extract_binary_from_initrd(&path, bi).map(|b| String::from_utf8_lossy(b).into_owned())
        }
    };
    let list: &str = match (&file, opt("modules.load")) { (_, Some(l)) => l, (Some(f), None) => f.as_str(), _ => return };
    for n in list.split(|c: char| c == ',' || c.is_ascii_whitespace()).filter(|n| !n.is_empty()) {
        if let Err(e) = load_by_name(n, Source::Initrd) {
            log(&alloc::format!("[kmod] modules.load {}: {:?}\n", n, e));
        }
    }
}
