//! `modules.alias` parsing (kernel side) and the OF string hash shared with the module macro.
//! Line forms (one per line, malformed lines skipped):
//! ```text
//! pci  <vid|*> <did|*> <name>
//! pci  <vid|*> <did|*> <class6> <mask6> <name>    class match, e.g. pci * * 010802 ffffff nvme
//! usb  <vid|*> <pid|*> <cc> <ss> <pp|*> <name>
//! of   <compatible> <name>                         exact string
//! sdio <vid> <did|*> <name>                        function-1 CIS manfid
//! ```
extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use crate::{KmodDevId, BUS_OF, BUS_PCI, BUS_SDIO, BUS_USB};

/// FNV-1a 32-bit, used for the `class` field of OF rows (`.kmod.ofstr` strings).
pub const fn fnv1a32(s: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    let mut i = 0;
    while i < s.len() { h ^= s[i] as u32; h = h.wrapping_mul(0x0100_0193); i += 1; }
    h
}

/// One parsed alias line. `compat` is set for `of` rows only.
pub struct Alias { pub id: KmodDevId, pub name: String, pub compat: String }

fn hex16(t: &str) -> Option<u16> { u16::from_str_radix(t, 16).ok() }
fn hex32(t: &str) -> Option<u32> { u32::from_str_radix(t, 16).ok() }
fn any16(s: &str) -> Option<u16> { if s == "*" { Some(0xffff) } else { hex16(s) } }
fn any_vid(s: &str) -> Option<u16> { any16(s) }

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 31 && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_compat(c: &str) -> bool {
    !c.is_empty() && c.len() <= 128 && c.bytes().all(|b| b.is_ascii_graphic())
}

pub fn parse_aliases(text: &str) -> Vec<Alias> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t: Vec<&str> = line.split_ascii_whitespace().collect();
        let mk = |id: KmodDevId, n: &str, compat: &str| Alias { id, name: String::from(n), compat: String::from(compat) };
        let a = match t.as_slice() {
            ["pci", v, d, n] => match (any_vid(v), any16(d)) {
                (Some(v), Some(d)) => Some(mk(KmodDevId { bus: BUS_PCI, a: v, b: d, c: 0, class: 0, class_mask: 0 }, n, "")),
                _ => None,
            },
            ["pci", v, d, cl, m, n] if cl.len() == 6 && m.len() == 6 => match (any_vid(v), any16(d), hex32(cl), hex32(m)) {
                (Some(v), Some(d), Some(cl), Some(m)) =>
                    Some(mk(KmodDevId { bus: BUS_PCI, a: v, b: d, c: 0, class: cl & m, class_mask: m }, n, "")),
                _ => None,
            },
            ["usb", v, p, cc, ss, pp, n] => {
                let (pp, mask_pp) = if *pp == "*" { (Some(0), 0) } else { (hex32(pp), 0xff) };
                match (any_vid(v), any16(p), hex32(cc), hex32(ss), pp) {
                    (Some(v), Some(p), Some(cc), Some(ss), Some(pp)) =>
                        Some(mk(KmodDevId { bus: BUS_USB, a: v, b: p, c: 0,
                            class: (cc << 16) | (ss << 8) | pp, class_mask: 0xffff00 | mask_pp }, n, "")),
                    _ => None,
                }
            }
            ["of", c, n] if valid_compat(c) => Some(mk(KmodDevId { bus: BUS_OF, a: 0, b: 0, c: c.len() as u16,
                class: fnv1a32(c.as_bytes()), class_mask: 0 }, n, c)),
            ["sdio", v, d, n] => match (hex16(v), any16(d)) {
                (Some(v), Some(d)) => Some(mk(KmodDevId { bus: BUS_SDIO, a: v, b: d, c: 0, class: 0, class_mask: 0 }, n, "")),
                _ => None,
            },
            _ => None,
        };
        if let Some(a) = a { if valid_name(&a.name) { out.push(a); } }
    }
    out
}

/// First module name matching a PCI function (vendor/device and/or class code `cc<<16|sc<<8|pi`).
pub fn match_pci<'a>(aliases: &'a [Alias], vendor: u16, device: u16, class: u32) -> Option<&'a str> {
    aliases.iter().find(|al| al.id.bus == BUS_PCI
        && (al.id.a == 0xffff || al.id.a == vendor) && (al.id.b == 0xffff || al.id.b == device)
        && (class & al.id.class_mask) == al.id.class).map(|al| al.name.as_str())
}

pub fn match_of<'a>(aliases: &'a [Alias], compat: &str) -> Option<&'a str> {
    aliases.iter().find(|al| al.id.bus == BUS_OF && al.compat == compat).map(|al| al.name.as_str())
}

pub fn match_sdio<'a>(aliases: &'a [Alias], vendor: u16, device: u16) -> Option<&'a str> {
    aliases.iter().find(|al| al.id.bus == BUS_SDIO && al.id.a == vendor && (al.id.b == 0xffff || al.id.b == device))
        .map(|al| al.name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    const T: &str = "pci 1af4 1042 virtio_blk\npci * * 010802 ffffff nvme\npci 8086 * 040300 ffff00 hda\n\
usb * * 08 06 50 usb_storage\nof brcm,2712-v3d v3d\nof leandros,test hello\nsdio 02d0 a9bf brcmf\nsdio 02d0 * brcmx\n";
    #[test] fn parses_all_forms() {
        let a = parse_aliases(T);
        assert_eq!(a.len(), 8);
        assert_eq!(match_pci(&a, 0x1af4, 0x1042, 0), Some("virtio_blk"));
        assert_eq!(match_pci(&a, 0x1234, 0x5678, 0x010802), Some("nvme"));
        assert_eq!(match_pci(&a, 0x1234, 0x5678, 0x010601), None);
        assert_eq!(match_pci(&a, 0x8086, 0x1, 0x040300), Some("hda"));
        assert_eq!(match_pci(&a, 0x8086, 0x1, 0x040380), Some("hda"));
        assert_eq!(match_pci(&a, 0x10de, 0x1, 0x040300), None);
        assert_eq!(match_of(&a, "leandros,test"), Some("hello"));
        assert_eq!(match_of(&a, "leandros,tes"), None);
        assert_eq!(match_sdio(&a, 0x02d0, 0xa9bf), Some("brcmf"));
        assert_eq!(match_sdio(&a, 0x02d0, 0x1111), Some("brcmx"));
        assert_eq!(match_sdio(&a, 0x1, 0xa9bf), None);
        assert_eq!(a[4].id.class, fnv1a32(b"brcm,2712-v3d"));
        assert_eq!(a[4].id.c, 13);
    }
    #[test] fn malformed_skipped() {
        let bad = "pci zz 1 x\npci 1 2\npci 1 2 0108 ff nvme\npci * * 01080 ffffff n\nof\nof a\nof a b c\n\
sdio 02d0\nsdio xx yy n\nusb 1 2 3\nfoo bar baz\npci 1 2 BAD-NAME\nof x UPPER\n\npci 1 2 ok_name\n";
        let a = parse_aliases(bad);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "ok_name");
    }
}
