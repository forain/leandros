//! Boot-time module policy: `kmod=0`, `modules.load=`, `modules.blacklist=`, and the
//! initrd lookup `lib/modules/<build-id>/<name>.kmod`.

use alloc::string::String;
use super::load::{load_image, KmodErr, Source};
use alloc::vec::Vec;
use kmod_abi::{KmodDevId, KmodId, BUS_PCI, BUS_USB};

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
    let list: &str = match (&file, opt("modules.load")) { (_, Some(l)) => l, (Some(f), None) => f.as_str(), _ => "" };
    for n in list.split(|c: char| c == ',' || c.is_ascii_whitespace()).filter(|n| !n.is_empty()) {
        if let Err(e) = load_by_name(n, Source::Initrd) {
            log(&alloc::format!("[kmod] modules.load {}: {:?}\n", n, e));
        }
    }
    autoload_aliases();
}

/// One parsed `modules.alias` line.
pub struct Alias { pub id: KmodDevId, pub name: String }

fn hex16(t: &str) -> Option<u16> { u16::from_str_radix(t, 16).ok() }
fn hex8(t: &str) -> Option<u32> { u32::from_str_radix(t, 16).ok() }

/// Parse the alias file (see plan section 3.5). Malformed lines are skipped.
/// `pci <vvvv> <dddd|*> <name>`; `usb <vvvv> <pppp|*> <cc> <ss> <pp|*> <name>`.
pub fn parse_aliases(text: &str) -> Vec<Alias> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t: Vec<&str> = line.split_ascii_whitespace().collect();
        let any16 = |s: &str| if s == "*" { Some(0xffffu16) } else { hex16(s) };
        let a = match t.as_slice() {
            ["pci", v, d, n] => match (hex16(v), any16(d)) {
                (Some(v), Some(d)) => Some(Alias {
                    id: KmodDevId { bus: BUS_PCI, a: v, b: d, c: 0, class: 0, class_mask: 0 },
                    name: String::from(*n) }),
                _ => None,
            },
            ["usb", v, p, cc, ss, pp, n] => {
                let (cc, ss) = (hex8(cc), hex8(ss));
                let (pp, mask_pp) = if *pp == "*" { (Some(0), 0) } else { (hex8(pp), 0xff) };
                match (hex16(v), any16(p), cc, ss, pp) {
                    (Some(v), Some(p), Some(cc), Some(ss), Some(pp)) => Some(Alias {
                        id: KmodDevId { bus: BUS_USB, a: v, b: p, c: 0,
                            class: (cc << 16) | (ss << 8) | pp,
                            class_mask: 0xffff00 | mask_pp },
                        name: String::from(*n) }),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(a) = a { if valid_name(&a.name) { out.push(a); } }
    }
    out
}

/// First alias name matching a PCI function.
pub fn match_pci<'a>(aliases: &'a [Alias], vendor: u16, device: u16) -> Option<&'a str> {
    aliases.iter()
        .find(|al| al.id.bus == BUS_PCI && al.id.a == vendor && (al.id.b == 0xffff || al.id.b == device))
        .map(|al| al.name.as_str())
}

/// For every PCI function, load the first matching alias name once. USB lines are
/// parsed and ignored until the USB hook exists. Policy (kmod=0, blacklist) is
/// enforced by `load_by_name`; an already-loaded name is skipped quietly.
fn autoload_aliases() {
    if super::disabled() { return; }
    let bi = crate::BOOT_INFO_PTR.load(core::sync::atomic::Ordering::SeqCst);
    if bi == 0 { return; }
    let bi = unsafe { &*(bi as *const boot::BootInfo) };
    let path = alloc::format!("lib/modules/{}/modules.alias", super::buildid::build_id_hex());
    let text = match crate::init::extract_binary_from_initrd(&path, bi) {
        Some(b) => String::from_utf8_lossy(b).into_owned(),
        None => return,
    };
    let aliases = parse_aliases(&text);
    let mut tried: Vec<String> = Vec::new();
    for d in drivers::pci::scan() {
        let Some(name) = match_pci(&aliases, d.vendor_id, d.device_id) else { continue };
        if tried.iter().any(|t| t == name) { continue; }
        tried.push(String::from(name));
        match load_by_name(name, Source::Initrd) {
            Ok(_) | Err(KmodErr::Exists) => {}
            Err(e) => log(&alloc::format!("[kmod] alias {:04x}:{:04x} {}: {:?}\n", d.vendor_id, d.device_id, name, e)),
        }
    }
}
