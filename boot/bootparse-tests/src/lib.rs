//! Host tests: feed synthetic boot blobs to the kernel's real parsers.

#[cfg(test)]
use std::sync::{Mutex, MutexGuard};
/// The parsers write into process-wide statics (memory map, command line):
/// serialize every test that calls one.
#[cfg(test)]
static LOCK: Mutex<()> = Mutex::new(());
#[cfg(test)]
fn lock() -> MutexGuard<'static, ()> { LOCK.lock().unwrap_or_else(|e| e.into_inner()) }

#[cfg(test)]
mod tests {
    use boot::{multiboot2, MemoryType};
    use super::lock;

    fn tag(buf: &mut Vec<u8>, typ: u32, body: &[u8]) {
        buf.extend(typ.to_le_bytes());
        buf.extend(((8 + body.len()) as u32).to_le_bytes());
        buf.extend(body);
        while buf.len() % 8 != 0 { buf.push(0); }
    }

    /// 8-byte aligned MBI: total_size, reserved, tags, end tag.
    fn mbi(tags: impl FnOnce(&mut Vec<u8>)) -> Vec<u64> {
        let mut b = vec![0u8; 8];
        tags(&mut b);
        tag(&mut b, 0, &[]);
        let n = b.len() as u32;
        b[0..4].copy_from_slice(&n.to_le_bytes());
        b.resize((b.len() + 7) & !7, 0);
        b.chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
    }

    fn efi_desc(typ: u32, start: u64, pages: u64) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend(typ.to_le_bytes()); d.extend(0u32.to_le_bytes());
        d.extend(start.to_le_bytes()); d.extend(0u64.to_le_bytes());
        d.extend(pages.to_le_bytes()); d.extend(0u64.to_le_bytes());
        d
    }

    #[test]
    fn cmdline_module_efi() {
        let _g = lock();
        let m = mbi(|b| {
            tag(b, 1, b"root=/dev/vda foo=bar\0");
            let mut md = Vec::new();
            md.extend(0x1000_0000u32.to_le_bytes());
            md.extend(0x1000_2000u32.to_le_bytes());
            md.extend(b"initrd\0");
            tag(b, 3, &md);
            let mut md2 = Vec::new(); // second module is ignored
            md2.extend(0x2000_0000u32.to_le_bytes());
            md2.extend(0x3000_0000u32.to_le_bytes());
            tag(b, 3, &md2);
            tag(b, 12, &0xdead_beef_0000u64.to_le_bytes());
        });
        let info = unsafe { multiboot2::parse(m.as_ptr() as usize) };
        assert_eq!(info.cmdline(), Some("root=/dev/vda foo=bar"));
        assert_eq!((info.initrd_base, info.initrd_size), (0x1000_0000, 0x2000));
        assert_eq!(info.efi_system_table, 0xdead_beef_0000);
        assert_eq!(info.memory_map_len, 0);
    }

    #[test]
    fn efi_memmap_when_no_tag6() {
        let _g = lock();
        let m = mbi(|b| {
            let mut body = Vec::new();
            body.extend(48u32.to_le_bytes()); // descriptor_size (padded)
            body.extend(1u32.to_le_bytes());
            for (t, s, p) in [(7u32, 0x10_0000u64, 256u64), (9, 0x20_0000, 4), (10, 0x30_0000, 2), (11, 0x40_0000, 1), (3, 0x50_0000, 8)] {
                let mut d = efi_desc(t, s, p);
                d.resize(48, 0);
                body.extend(d);
            }
            tag(b, 17, &body);
        });
        let info = unsafe { multiboot2::parse(m.as_ptr() as usize) };
        let r = info.memory_regions();
        assert_eq!(r.len(), 5);
        assert_eq!((r[0].base, r[0].length, r[0].kind), (0x10_0000, 256 * 4096, MemoryType::Available));
        assert_eq!(r[1].kind, MemoryType::AcpiReclaimable);
        assert_eq!(r[2].kind, MemoryType::AcpiNvs);
        assert_eq!(r[3].kind, MemoryType::Reserved);
        assert_eq!(r[4].kind, MemoryType::Available);
    }

    #[test]
    fn tag6_wins_over_efi() {
        let _g = lock();
        let m = mbi(|b| {
            let mut body = Vec::new();
            body.extend(24u32.to_le_bytes()); body.extend(0u32.to_le_bytes());
            body.extend(0x10_0000u64.to_le_bytes()); body.extend(0x1000u64.to_le_bytes());
            body.extend(1u32.to_le_bytes()); body.extend(0u32.to_le_bytes());
            tag(b, 6, &body);
            let mut e = Vec::new();
            e.extend(32u32.to_le_bytes()); e.extend(1u32.to_le_bytes());
            e.extend(efi_desc(7, 0x9000_0000, 1));
            tag(b, 17, &e);
        });
        let info = unsafe { multiboot2::parse(m.as_ptr() as usize) };
        assert_eq!(info.memory_regions().len(), 1);
        assert_eq!(info.memory_regions()[0].base, 0x10_0000);
    }

    #[test]
    fn pvh_start_info() {
        let _g = lock();
        let cmd = b"console=ttyS0 x=1\0".to_vec();
        let modlist: Vec<u64> = vec![0x2000_0000, 0x4000, 0, 0];
        // hvm_memmap_table_entry: addr, size, type | reserved << 32
        let memmap: Vec<u64> = vec![
            0, 0x9fc00, 1,
            0x10_0000, 0x7ff0_0000, 1,
            0xb000_0000, 0x1000_0000, 2,
            0x1_0000_0000, 0x1_0000_0000, 1,
        ];
        let mut si: Vec<u64> = vec![0; 7];
        si[0] = multiboot2::PVH_START_INFO_MAGIC as u64 | (1u64 << 32); // magic, version
        si[1] = 1u64 << 32; // flags = 0, nr_modules = 1
        si[2] = modlist.as_ptr() as u64;
        si[3] = cmd.as_ptr() as u64;
        si[4] = 0xf0000;
        si[5] = memmap.as_ptr() as u64;
        si[6] = 4; // memmap_entries
        let info = unsafe { multiboot2::parse_pvh(si.as_ptr() as usize) };
        assert_eq!(info.cmdline(), Some("console=ttyS0 x=1"));
        assert_eq!((info.initrd_base, info.initrd_size), (0x2000_0000, 0x4000));
        assert_eq!(info.rsdp_addr, 0xf0000);
        let r = info.memory_regions();
        assert_eq!(r.len(), 4);
        assert_eq!((r[3].base, r[3].length, r[3].kind), (0x1_0000_0000, 0x1_0000_0000, MemoryType::Available));
        assert_eq!(r[2].kind, MemoryType::Reserved);
    }
}

#[cfg(test)]
mod builders {
    use super::lock;
    use boot::fdt_build::*;
    use boot::mb2_build::*;
    use boot::{device_tree, multiboot2, MemoryType};

    const GIB: u64 = 1 << 30;

    fn raw(base: u64, length: u64, raw_type: u32) -> RawRegion { RawRegion { base, length, raw_type } }

    /// 8-byte aligned buffer.
    fn abuf(n: usize) -> Vec<u64> { vec![0u64; (n + 7) / 8] }
    fn bytes(v: &mut Vec<u64>) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, v.len() * 8) }
    }

    fn limine_map() -> Vec<RawRegion> {
        vec![
            raw(0, 0x9f000, 0),                 // low RAM (page 0 Reserved)
            raw(0x9f000, 0x61000, 1),
            raw(0x10_0000, 0x7f0_0000, 0),      // usable
            raw(0x800_0000, 0x10_0000, 5),      // bootloader reclaimable
            raw(0x810_0000, 0x20_0000, 6),      // old kernel+modules
            raw(0x830_0000, 0x1000, 2),         // ACPI reclaimable
            raw(0x831_0000, 0x1000, 3),         // ACPI NVS
            raw(0x832_0000, 0x1000, 4),         // bad
            raw(0x840_0000, 0x1000, 7),         // framebuffer
            raw(GIB, 2 * GIB, 0),               // straddles 2 GiB: all of it Available
            raw(4 * GIB, 4 * GIB, 0),           // above 4 GiB, up to 8 GiB
        ]
    }

    #[test]
    fn mb2_roundtrip() {
        let _g = lock();
        let regions = limine_map();
        let mut rsdp = [0u8; 36];
        rsdp[..8].copy_from_slice(b"RSD PTR ");
        rsdp[15] = 2;
        rsdp[24..32].copy_from_slice(&0xdead_0000u64.to_le_bytes());
        let reserve = [(0x10_0000u64, 0x40_0000u64)]; // new kernel image
        let input = Mb2Input {
            cmdline: b"root=/dev/vda quiet",
            loader_name: b"LeandrOS kexec",
            initrd: Some((0x1000_0000, 0x1234)),
            regions: &regions,
            reserve: &reserve,
            framebuffer: Some(Mb2Fb { addr: 0xfd00_0000, pitch: 4096, width: 1024, height: 768, bpp: 32 }),
            rsdp: Some(&rsdp),
            efi64: 0xabcd_0000,
        };
        let mut scratch = Box::new(Mb2Scratch::new());
        let mut buf = abuf(8192);
        let n = build_mbi(&input, &mut scratch, bytes(&mut buf)).unwrap();
        assert_eq!(n % 8, 0);
        assert_eq!(u32::from_le_bytes(bytes(&mut buf)[0..4].try_into().unwrap()) as usize, n);

        let info = unsafe { multiboot2::parse(buf.as_ptr() as usize) };
        assert_eq!(info.cmdline(), Some("root=/dev/vda quiet"));
        assert_eq!((info.initrd_base, info.initrd_size), (0x1000_0000, 0x1234));
        assert_eq!(info.efi_system_table, 0xabcd_0000);
        assert_eq!((info.framebuffer_base, info.framebuffer_pitch, info.framebuffer_width, info.framebuffer_height),
                   (0xfd00_0000, 4096, 1024, 768));
        // RSDP is a pointer into the MBI copy.
        let p = info.rsdp_addr as *const u8;
        assert_eq!(unsafe { std::slice::from_raw_parts(p, 36) }, &rsdp[..]);
        assert!(info.rsdp_addr as usize > buf.as_ptr() as usize);
        assert!((info.rsdp_addr as usize) < buf.as_ptr() as usize + n);

        let r = info.memory_regions();
        assert!(r.len() <= 128);
        for w in r.windows(2) { assert!(w[0].end() <= w[1].base); }
        let kind_at = |a: u64| r.iter().find(|e| e.base <= a && a < e.end()).map(|e| e.kind);
        // Below 1 MiB: page 0 Reserved, usable low RAM passed through.
        assert_eq!(kind_at(0x1000), Some(MemoryType::Available));
        assert_eq!(kind_at(0xff000), Some(MemoryType::Reserved));
        // New kernel image reserved over Limine-usable RAM.
        assert_eq!(kind_at(0x20_0000), Some(MemoryType::Reserved));
        assert_eq!(kind_at(0x50_0000), Some(MemoryType::Available));
        // Initrd reserved (page rounded).
        assert_eq!(kind_at(0x1000_0000 + 0x1000), Some(MemoryType::Reserved));
        // Raw 5 and 6 become Available; 2/3/4/7 map through.
        assert_eq!(kind_at(0x800_0000), Some(MemoryType::Available));
        assert_eq!(kind_at(0x810_0000), Some(MemoryType::Available));
        assert_eq!(kind_at(0x830_0000), Some(MemoryType::AcpiReclaimable));
        assert_eq!(kind_at(0x831_0000), Some(MemoryType::AcpiNvs));
        assert_eq!(kind_at(0x832_0000), Some(MemoryType::BadMemory));
        assert_eq!(kind_at(0x840_0000), Some(MemoryType::Reserved));
        // No 2 GiB clamp any more (the direct kernel maps all RAM): RAM
        // across 2 GiB and above 4 GiB stays Available, the hole does not.
        assert_eq!(kind_at(2 * GIB - 0x1000), Some(MemoryType::Available));
        assert_eq!(kind_at(2 * GIB), Some(MemoryType::Available));
        assert_eq!(kind_at(3 * GIB - 0x1000), Some(MemoryType::Available));
        assert_eq!(kind_at(3 * GIB), None);
        assert_eq!(kind_at(5 * GIB), Some(MemoryType::Available));
        assert_eq!(kind_at(8 * GIB - 0x1000), Some(MemoryType::Available));
        let avail: u64 = r.iter().filter(|e| e.kind == MemoryType::Available)
            .map(|e| e.end().min(8 * GIB).saturating_sub(e.base.max(2 * GIB))).sum();
        assert_eq!(avail, GIB + 4 * GIB);
        // Low RAM keeps the loader's types (AP trampoline at 0x7000 must be
        // Available), page 0 and the BIOS area stay Reserved.
        assert_eq!(kind_at(0x0), Some(MemoryType::Reserved));
        assert_eq!(kind_at(0x7000), Some(MemoryType::Available));
        assert_eq!(kind_at(0x9f000), Some(MemoryType::Reserved));
        assert!(r.iter().all(|e| e.kind != MemoryType::Available || e.base >= 0x1000));
    }

    #[test]
    fn mb2_minimal_acpi_v1_and_errors() {
        let _g = lock();
        let regions = [raw(0x10_0000, 0x1000_0000, 0)];
        let mut rsdp = [0u8; 36];
        rsdp[..8].copy_from_slice(b"RSD PTR ");
        let input = Mb2Input {
            cmdline: b"", loader_name: b"", initrd: None, regions: &regions, reserve: &[],
            framebuffer: None, rsdp: Some(&rsdp), efi64: 0,
        };
        let mut scratch = Box::new(Mb2Scratch::new());
        let mut buf = abuf(4096);
        let n = build_mbi(&input, &mut scratch, bytes(&mut buf)).unwrap();
        let info = unsafe { multiboot2::parse(buf.as_ptr() as usize) };
        assert_eq!(info.cmdline(), None);
        assert_eq!(info.initrd_base, 0);
        assert_ne!(info.rsdp_addr, 0);
        assert_eq!(info.memory_regions().len(), 2); // reserved page 0 + available
        let mut tiny = abuf(32);
        assert_eq!(build_mbi(&input, &mut scratch, bytes(&mut tiny)), Err(Mb2Error::BufferTooSmall));
        assert!(n > 32);
        let hi = Mb2Input { initrd: Some((0xffff_f000, 0x2000)), ..input };
        assert_eq!(build_mbi(&hi, &mut scratch, bytes(&mut buf)), Err(Mb2Error::InitrdAbove4G));
    }

    #[test]
    fn mb2_caps_at_128_entries() {
        let _g = lock();
        // 300 usable 1 MiB pieces separated by 1 MiB holes (below 2 GiB).
        let mut regions = Vec::new();
        for i in 0..300u64 {
            regions.push(raw(0x10_0000 + i * 0x20_0000, 0x10_0000, 0));
        }
        let input = Mb2Input {
            cmdline: b"x", loader_name: b"", initrd: None, regions: &regions, reserve: &[],
            framebuffer: None, rsdp: None, efi64: 0,
        };
        let mut scratch = Box::new(Mb2Scratch::new());
        let mut buf = abuf(16384);
        build_mbi(&input, &mut scratch, bytes(&mut buf)).unwrap();
        let info = unsafe { multiboot2::parse(buf.as_ptr() as usize) };
        assert!(info.memory_regions().len() <= 128);
        assert!(info.memory_regions().iter().any(|r| r.kind == MemoryType::Available));
    }

    #[test]
    fn mbi_placement_avoids_low_users() {
        let a = pick_mbi_phys(0x1000, &[]).unwrap();
        assert!(a < 0x100000 && a % 0x1000 == 0);
        for &(s, e) in MB2_LOW_USED.iter() {
            assert!(a + 0x1000 <= s || a >= e, "{a:#x} overlaps {s:#x}-{e:#x}");
        }
        let b = pick_mbi_phys(0x20000, &[]).unwrap();
        for &(s, e) in MB2_LOW_USED.iter() {
            assert!(b + 0x20000 <= s || b >= e);
        }
        assert!(pick_mbi_phys(0x80000, &[]).is_none());
        let c = pick_mbi_phys(0x1000, &[(a, a + 0x1000)]).unwrap();
        assert_ne!(a, c);
        for sz in [0x100usize, 0x1000, 0x3000, 0x10000] {
            let p = pick_mbi_phys(sz, &[]).unwrap() as usize;
            for bad in [0x7000usize, 0x10000, 0x20000, 0x27fff, 0x90000] {
                assert!(!(p <= bad && bad < p + sz), "sz {sz:#x} at {p:#x} hits {bad:#x}");
            }
        }
    }

    #[test]
    fn fdt_roundtrip() {
        let _g = lock();
        let memory = [(0x4000_0000u64, 0x3000_0000u64), (0x8000_0000, 0x1_0000_0000)];
        let cpus = [0u64, 1, 2, 3];
        let input = FdtInput {
            bootargs: b"root=/dev/vda console=ttyAMA0",
            initrd: Some((0x4800_0000, 0x1_2345)),
            rng_seed: &[7u8; 32],
            memory: &memory,
            uart_base: 0x0900_0000,
            pci_ecam_base: 0x40_1000_0000,
            ecam_size: 0x1000_0000,
            framebuffer: Some(FdtFb { addr: 0x5000_0000, width: 800, height: 600, stride: 3200 }),
            cpus: &cpus,
            psci_method: b"hvc",
            kexec_ap_mailbox: 0,
        };
        let mut buf = abuf(8192);
        let n = build_fdt(&input, bytes(&mut buf)).unwrap();
        assert!(n < 8192 && n % 4 == 0);
        let info = unsafe { device_tree::parse(buf.as_ptr() as usize) };
        assert_eq!(info.cmdline(), Some("root=/dev/vda console=ttyAMA0"));
        assert_eq!((info.initrd_base, info.initrd_size), (0x4800_0000, 0x1_2345));
        assert_eq!(info.uart_base, 0x0900_0000);
        assert_eq!(info.pci_ecam_base, 0x40_1000_0000);
        assert_eq!((info.framebuffer_base, info.framebuffer_width, info.framebuffer_height, info.framebuffer_pitch),
                   (0x5000_0000, 800, 600, 3200));
        let r = info.memory_regions();
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].base, r[0].length, r[0].kind), (0x4000_0000, 0x3000_0000, MemoryType::Available));
        assert_eq!((r[1].base, r[1].length), (0x8000_0000, 0x1_0000_0000));
        let mut seed = [0u8; 64];
        assert_eq!(device_tree::take_rng_seed(&mut seed), 32);
        assert_eq!(&seed[..32], &[7u8; 32]);
        let mut got = Vec::new();
        assert!(unsafe { device_tree::for_each_cpu_mpidr_at(buf.as_ptr() as usize, &mut |m| got.push(m)) });
        assert_eq!(got, vec![0, 1, 2, 3]);
        let b = bytes(&mut buf);
        assert_eq!(&b[0..4], &0xD00D_FEEDu32.to_be_bytes());
        assert_eq!(u32::from_be_bytes(b[4..8].try_into().unwrap()) as usize, n);
    }

    #[test]
    fn fdt_minimal_wide_mpidr_and_limits() {
        let _g = lock();
        let input = FdtInput {
            bootargs: b"", initrd: None, rng_seed: &[], memory: &[(0x4000_0000, 0x1000_0000)],
            uart_base: 0, pci_ecam_base: 0, ecam_size: 0, framebuffer: None,
            cpus: &[0x1_0000_0001], psci_method: b"", kexec_ap_mailbox: 0,
        };
        let mut buf = abuf(2048);
        build_fdt(&input, bytes(&mut buf)).unwrap();
        let info = unsafe { device_tree::parse(buf.as_ptr() as usize) };
        assert_eq!(info.cmdline(), None);
        assert_eq!(info.initrd_base, 0);
        assert_eq!(info.uart_base, 0);
        assert_eq!(info.memory_regions().len(), 1);
        let mut got = Vec::new();
        unsafe { device_tree::for_each_cpu_mpidr_at(buf.as_ptr() as usize, &mut |m| got.push(m)) };
        assert_eq!(got, vec![0x1_0000_0001]);

        let mut tiny = abuf(64);
        assert_eq!(build_fdt(&input, bytes(&mut tiny)), Err(FdtError::BufferTooSmall));
        let many = vec![(0x4000_0000u64, 0x1000u64); 61];
        let too = FdtInput { memory: &many, ..input };
        assert_eq!(build_fdt(&too, bytes(&mut buf)), Err(FdtError::TooManyMemory));
    }

    #[test]
    fn fdt_60_regions_and_select_memory() {
        let _g = lock();
        use boot::MemoryRegion;
        let mut regs = Vec::new();
        for i in 0..70u64 {
            regs.push(MemoryRegion { base: 0x4000_0000 + i * 0x100_0000, length: 0x1000 * (i + 1), kind: MemoryType::Available });
        }
        regs.push(MemoryRegion { base: 17 * GIB, length: GIB, kind: MemoryType::Available });
        regs.push(MemoryRegion { base: 0x2000_0000, length: GIB, kind: MemoryType::Reserved });
        let mut sel = [(0u64, 0u64); FDT_MAX_MEMORY];
        let n = select_memory(&regs, &mut sel);
        assert_eq!(n, 60);
        assert!(sel[..n].iter().all(|&(b, _)| b < 16 * GIB && b >= 0x4000_0000));
        assert!(sel[..n].iter().all(|&(_, l)| l >= 0x1000 * 11)); // 10 smallest dropped
        let input = FdtInput {
            bootargs: b"a", initrd: None, rng_seed: &[], memory: &sel[..n],
            uart_base: 0x0900_0000, pci_ecam_base: 0, ecam_size: 0, framebuffer: None,
            cpus: &[0], psci_method: b"hvc", kexec_ap_mailbox: 0,
        };
        let mut buf = abuf(8192);
        build_fdt(&input, bytes(&mut buf)).unwrap();
        let info = unsafe { device_tree::parse(buf.as_ptr() as usize) };
        assert_eq!(info.memory_regions().len(), 60);
    }
}
