//! ACPI table page collector. Walks RSDP -> XSDT/RSDT -> every table (+ the
//! FADT's DSDT and FACS) and reports each physical range it needs to read.
//!
//! The walk is generic over memory access so the direct-boot kernel can map
//! each range just before reading it (`want` maps, `read` copies) and the
//! host tests can run it on a synthetic physical memory image. `want` is
//! always called for a range BEFORE `read` touches it.

/// Longest table accepted (matches `acpi::for_each_table`).
pub const MAX_TABLE: u64 = 16 << 20;

fn u32_at(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// Request + read a table header at `phys`, then request its full length.
/// Returns (signature, length) of a plausible table.
fn table(phys: u64, max_phys: u64, want: &mut dyn FnMut(u64, u64), read: &mut dyn FnMut(u64, &mut [u8]))
    -> Option<([u8; 4], u64)>
{
    if phys == 0 || phys >= max_phys { return None; }
    want(phys, 36);
    let mut h = [0u8; 36];
    read(phys, &mut h);
    let len = u32_at(&h, 4) as u64;
    if len < 36 || len > MAX_TABLE || phys.checked_add(len)? > max_phys { return None; }
    want(phys, len);
    Some(([h[0], h[1], h[2], h[3]], len))
}

/// Walk the tables reachable from `rsdp`. Every range passed to `want` is a
/// range the ACPI readers dereference.
pub fn walk(rsdp: u64, max_phys: u64, want: &mut dyn FnMut(u64, u64), read: &mut dyn FnMut(u64, &mut [u8])) {
    if rsdp == 0 || rsdp >= max_phys { return; }
    want(rsdp, 36);
    let mut r = [0u8; 36];
    read(rsdp, &mut r);
    if &r[..8] != b"RSD PTR " { return; }
    let (root, es) = if r[15] >= 2 && u64_at(&r, 24) != 0 { (u64_at(&r, 24), 8usize) } else { (u32_at(&r, 16) as u64, 4usize) };
    let Some((_, rlen)) = table(root, max_phys, want, read) else { return };
    let n = (rlen as usize - 36) / es;
    for i in 0..n {
        let mut e = [0u8; 8];
        read(root + 36 + (i * es) as u64, &mut e[..es]);
        let phys = if es == 8 { u64::from_le_bytes(e) } else { u32::from_le_bytes([e[0], e[1], e[2], e[3]]) as u64 };
        let Some((sig, len)) = table(phys, max_phys, want, read) else { continue };
        if &sig == b"FACP" && len >= 44 {
            let mut f = [0u8; 148];
            let m = (len as usize).min(148);
            read(phys, &mut f[..m]);
            let mut dsdt = u32_at(&f, 40) as u64;
            if m >= 148 && u64_at(&f, 140) != 0 { dsdt = u64_at(&f, 140); }
            table(dsdt, max_phys, want, read);
            let mut facs = u32_at(&f, 36) as u64;
            if m >= 140 && u64_at(&f, 132) != 0 { facs = u64_at(&f, 132); }
            if facs != 0 && facs < max_phys {
                want(facs, 64);
                let mut h = [0u8; 8];
                read(facs, &mut h);
                let fl = u32_at(&h, 4) as u64;
                if fl > 64 && fl <= MAX_TABLE && facs + fl <= max_phys { want(facs, fl); }
            }
        }
    }
}
