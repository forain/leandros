//! Boot-time module policy: `kmod=0`, `modules.load=`, `modules.blacklist=`, and the
//! initrd lookup `lib/modules/<build-id>/<name>.kmod`.

use alloc::string::String;
use super::load::{load_image, KmodErr, Source};
use alloc::vec::Vec;
use kmod_abi::{KmodId, BUS_USB};

fn log(s: &str) { drivers::pci::serial_debug(s); }

fn cmdline() -> &'static str { crate::boot_cmdline().unwrap_or("") }

/// The value of `key=` on the command line (last occurrence wins).
pub(super) fn opt(key: &str) -> Option<&'static str> {
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

use kmod_abi::alias::valid_name;

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
    }
    if super::disabled() {
        // M6: a direct-boot (kexec/Pi) kernel can lose the window; with boot modules listed that
        // means no root disk, so say so loudly instead of a silent mount timeout.
        let bi = crate::BOOT_INFO_PTR.load(core::sync::atomic::Ordering::SeqCst);
        if bi != 0 {
            let bi = unsafe { &*(bi as *const boot::BootInfo) };
            let path = alloc::format!("lib/modules/{}/modules.load", super::buildid::build_id_hex());
            if let Some(b) = crate::init::extract_binary_from_initrd(&path, bi) {
                let t = String::from_utf8_lossy(b);
                if !t.trim().is_empty() {
                    log(&alloc::format!("[kmod] ERROR: kmod is disabled but modules.load lists: {}\n", t.trim().replace('\n', " ")));
                }
            }
        }
        return;
    }
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
    let list: &str = match (&file, opt("modules.load")) { (_, Some(l)) => l, (Some(f), None) => f.as_str(), _ => "" };
    for n in list.split(|c: char| c == ',' || c.is_ascii_whitespace()).filter(|n| !n.is_empty()) {
        if let Err(e) = load_by_name(n, Source::Initrd) {
            log(&alloc::format!("[kmod] modules.load {}: {:?}\n", n, e));
        }
    }
    autoload_aliases();
    usb_selftest();
    plat_selftest();
    // Boot-path modules (root disks) register disks late: re-check for the boot-log partition.
    crate::klog::on_disk_registered();
}

pub use kmod_abi::alias::{match_of, match_pci, match_sdio, parse_aliases, Alias};

/// The initrd's parsed `modules.alias`, read once on first use.
static ALIASES: spin::Mutex<Option<Vec<Alias>>> = spin::Mutex::new(None);

fn with_aliases<R>(f: impl FnOnce(&[Alias]) -> R) -> Option<R> {
    let mut g = ALIASES.lock();
    if g.is_none() {
        let bi = crate::BOOT_INFO_PTR.load(core::sync::atomic::Ordering::SeqCst);
        if bi == 0 { return None; }
        let bi = unsafe { &*(bi as *const boot::BootInfo) };
        let path = alloc::format!("lib/modules/{}/modules.alias", super::buildid::build_id_hex());
        let text = crate::init::extract_binary_from_initrd(&path, bi)
            .map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
        *g = Some(parse_aliases(&text));
    }
    Some(f(g.as_ref().unwrap()))
}

fn load_logged(what: &str, name: &str) {
    match load_by_name(name, Source::Initrd) {
        Ok(_) => log(&alloc::format!("[kmod] {} -> {}\n", what, name)),
        Err(KmodErr::Exists) => {}
        Err(e) => log(&alloc::format!("[kmod] alias {} {}: {:?}\n", what, name, e)),
    }
}

/// For every PCI function, load the first matching alias name once (vendor/device or class).
/// USB lines are matched at interface enumerate by [`usb_hook`]; `of` lines by [`of_hook`].
/// Policy (kmod=0, blacklist) is enforced by `load_by_name`; an already-loaded name is skipped.
fn autoload_aliases() {
    if super::disabled() || opt("kmod") == Some("0") { return; }
    let mut tried: Vec<String> = Vec::new();
    for d in drivers::pci::scan() {
        let class = (d.class as u32) << 16 | (d.subclass as u32) << 8 | d.prog_if as u32;
        let Some(Some(name)) = with_aliases(|al| match_pci(al, d.vendor_id, d.device_id, class).map(String::from)) else { continue };
        if tried.iter().any(|t| *t == name) { continue; }
        tried.push(name.clone());
        load_logged(&alloc::format!("pci {:04x}:{:04x}", d.vendor_id, d.device_id), &name);
    }
}

/// USB alias autoload, called by the xhci enumerator for each interface nobody wants yet (task
/// context, before the device is configured). Loads the first module whose `usb` alias matches;
/// the module registers its class driver from `init`, so the enumerator's next `wants` succeeds.
pub fn usb_hook(vid: u16, pid: u16, class: u8, sub: u8, proto: u8) {
    // The hook can fire before `boot()` has parsed kmod=0, so honour it here too.
    if super::disabled() || opt("kmod") == Some("0") { return; }
    let intf = (class as u32) << 16 | (sub as u32) << 8 | proto as u32;
    let name = with_aliases(|al| al.iter().find(|al| al.id.bus == BUS_USB
        && (al.id.a == 0xffff || al.id.a == vid) && (al.id.b == 0xffff || al.id.b == pid)
        && (intf & al.id.class_mask) == (al.id.class & al.id.class_mask)).map(|al| al.name.clone())).flatten();
    let Some(name) = name else { return };
    load_logged(&alloc::format!("usb {:04x}:{:04x} {:02x}{:02x}{:02x}", vid, pid, class, sub, proto), &name);
}

/// Platform-registry hook: an announced device with this `compatible` found no built-in driver.
pub fn of_hook(compat: &str) {
    if super::disabled() || opt("kmod") == Some("0") { return; }
    let Some(Some(name)) = with_aliases(|al| match_of(al, compat).map(String::from)) else { return };
    load_logged(&alloc::format!("of {}", compat), &name);
}

/// SDIO hook: function-1 manfid of a card no built-in driver claimed.
pub fn sdio_hook(vid: u16, did: u16) {
    if super::disabled() || opt("kmod") == Some("0") { return; }
    let Some(Some(name)) = with_aliases(|al| match_sdio(al, vid, did).map(String::from)) else { return };
    load_logged(&alloc::format!("sdio {:04x}:{:04x}", vid, did), &name);
}

/// `usb.alias_selftest=1`: run the enumerate-time hook against a synthetic video-streaming
/// interface (QEMU has no UVC device), the way the xhci enumerator does for a real camera.
fn usb_selftest() {
    // `=bt`: a synthetic Bluetooth HCI interface (e0/01/01), QEMU has no USB BT controller.
    let bt = opt("usb.alias_selftest") == Some("bt");
    if opt("usb.alias_selftest") != Some("1") && !bt { return; }
    let cfg: [u8; 9] = if bt { [9, 4, 0, 0, 3, 0xE0, 0x01, 0x01, 0] } else { [9, 4, 1, 0, 0, 0x0E, 0x02, 0x00, 0] };
    let r = usb::class::wants_or_load(&cfg, 0x1234, 0x5678);
    log(&alloc::format!("[kmod] usb alias selftest: class driver wants the interface: {:?}\n", r));
}

/// `plat.alias_selftest=1`: announce a synthetic platform device with compatible
/// `leandros,test` that no built-in driver claims, the way the DT walker would.
fn plat_selftest() {
    if opt("plat.alias_selftest") != Some("1") { return; }
    drivers::platform::announce("leandros,test", drivers::platform::PlatDev::default());
    log("[kmod] plat alias selftest: announced leandros,test\n");
}
