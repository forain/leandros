//! Host tests: feed synthetic boot blobs to the kernel's real parsers.

#[cfg(test)]
mod tests {
    use boot::{multiboot2, MemoryType};

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
        let cmd = b"console=ttyS0 x=1\0".to_vec();
        let modlist: Vec<u64> = vec![0x2000_0000, 0x4000, 0, 0];
        let mut si: Vec<u64> = vec![0; 5];
        si[0] = multiboot2::PVH_START_INFO_MAGIC as u64 | (1u64 << 32); // magic, version
        si[1] = 1u64 << 32; // flags = 0, nr_modules = 1
        si[2] = modlist.as_ptr() as u64;
        si[3] = cmd.as_ptr() as u64;
        si[4] = 0xf0000;
        let info = unsafe { multiboot2::parse_pvh(si.as_ptr() as usize) };
        assert_eq!(info.cmdline(), Some("console=ttyS0 x=1"));
        assert_eq!((info.initrd_base, info.initrd_size), (0x2000_0000, 0x4000));
        assert_eq!(info.rsdp_addr, 0xf0000);
    }
}
