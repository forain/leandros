//! Host tests for the kernel's partition scanner. The module under test is
//! included verbatim, so these exercise exactly the code the kernel runs.

#[path = "../../src/partscan.rs"]
pub mod partscan;

#[cfg(test)]
mod tests {
    use super::partscan::*;

    const SS: usize = 512;

    /// An in-memory disk of `nsec` sectors of `ss` bytes.
    struct Disk { ss: usize, data: Vec<u8> }

    impl Disk {
        fn new(ss: usize, nsec: u64) -> Self { Disk { ss, data: vec![0u8; ss * nsec as usize] } }
        fn nsec(&self) -> u64 { (self.data.len() / self.ss) as u64 }
        fn at(&mut self, lba: u64) -> &mut [u8] {
            let o = lba as usize * self.ss;
            &mut self.data[o..]
        }
        fn scan(&self) -> ScanResult {
            let ss = self.ss;
            scan(ss as u32, self.nsec(), |lba, buf| {
                let o = lba as usize * ss;
                if o + buf.len() > self.data.len() { return false; }
                buf.copy_from_slice(&self.data[o..o + buf.len()]);
                true
            })
        }
    }

    fn mbr_entry(d: &mut Disk, i: usize, status: u8, ptype: u8, start: u32, num: u32) {
        let s0 = d.at(0);
        let o = 446 + 16 * i;
        s0[o] = status;
        s0[o + 4] = ptype;
        s0[o + 8..o + 12].copy_from_slice(&start.to_le_bytes());
        s0[o + 12..o + 16].copy_from_slice(&num.to_le_bytes());
        s0[510] = 0x55;
        s0[511] = 0xAA;
    }

    const ESP_GUID: [u8; 16] = [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11,
                                0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B];

    /// Write a GPT header at `hdr_lba` with its entry array at `ent_lba`.
    fn write_gpt_copy(d: &mut Disk, hdr_lba: u64, alt_lba: u64, ent_lba: u64,
                      entries: &[([u8; 16], [u8; 16], u64, u64)]) {
        let ss = d.ss;
        let nent = 128u32;
        let esize = 128usize;
        let mut arr = vec![0u8; nent as usize * esize];
        for (i, (t, u, first, last)) in entries.iter().enumerate() {
            let e = &mut arr[i * esize..];
            e[0..16].copy_from_slice(t);
            e[16..32].copy_from_slice(u);
            e[32..40].copy_from_slice(&first.to_le_bytes());
            e[40..48].copy_from_slice(&last.to_le_bytes());
        }
        let ecrc = crc32(&arr);
        d.at(ent_lba)[..arr.len()].copy_from_slice(&arr);
        let nsec = d.nsec();
        let entry_secs = (arr.len() + ss - 1) / ss;
        let mut h = vec![0u8; 92];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&hdr_lba.to_le_bytes());
        h[32..40].copy_from_slice(&alt_lba.to_le_bytes());
        h[40..48].copy_from_slice(&(2 + entry_secs as u64).to_le_bytes());
        h[48..56].copy_from_slice(&(nsec - 2 - entry_secs as u64).to_le_bytes());
        h[56..72].copy_from_slice(&[0x11u8; 16]);
        h[72..80].copy_from_slice(&ent_lba.to_le_bytes());
        h[80..84].copy_from_slice(&nent.to_le_bytes());
        h[84..88].copy_from_slice(&(esize as u32).to_le_bytes());
        h[88..92].copy_from_slice(&ecrc.to_le_bytes());
        let c = crc32(&h);
        h[16..20].copy_from_slice(&c.to_le_bytes());
        d.at(hdr_lba)[..92].copy_from_slice(&h);
    }

    /// A protective MBR plus primary and backup GPT.
    fn make_gpt(ss: usize, nsec: u64, entries: &[([u8; 16], [u8; 16], u64, u64)]) -> Disk {
        let mut d = Disk::new(ss, nsec);
        mbr_entry(&mut d, 0, 0, 0xEE, 1, (nsec - 1).min(u32::MAX as u64) as u32);
        let entry_secs = (128 * 128 + ss - 1) / ss;
        write_gpt_copy(&mut d, 1, nsec - 1, 2, entries);
        write_gpt_copy(&mut d, nsec - 1, 1, nsec - 1 - entry_secs as u64, entries);
        d
    }

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn blank_disk_has_no_table() {
        let d = Disk::new(SS, 4096);
        assert_eq!(d.scan(), ScanResult { table: Table::None, parts: vec![] });
    }

    #[test]
    fn raw_f2fs_image_has_no_table() {
        // f2fs puts its superblock at byte 1024 and leaves sector 0 zero.
        let mut d = Disk::new(SS, 4096);
        d.at(2)[0..4].copy_from_slice(&0xF2F5_2010u32.to_le_bytes());
        assert_eq!(d.scan().table, Table::None);
    }

    #[test]
    fn mbr_primary_entries() {
        let mut d = Disk::new(SS, 1 << 20);
        d.at(0)[440..444].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        mbr_entry(&mut d, 0, 0x80, 0x0C, 2048, 204800);
        mbr_entry(&mut d, 1, 0x00, 0x83, 206848, 409600);
        mbr_entry(&mut d, 2, 0x00, 0x05, 616448, 1000); // extended: skipped
        let r = d.scan();
        assert_eq!(r.table, Table::Mbr { disk_sig: 0xDEAD_BEEF });
        assert_eq!(r.parts.len(), 2);
        assert_eq!(r.parts[0].pno, 1);
        assert_eq!(r.parts[0].start_lba, 2048);
        assert_eq!(r.parts[0].ptype, PartType::Mbr(0x0C));
        assert!(!r.parts[0].ptype.is_linux_fs());
        assert_eq!(r.parts[1].pno, 2);
        assert_eq!(r.parts[1].num_lbas, 409600);
        assert!(r.parts[1].ptype.is_linux_fs());
        assert!(r.parts[1].aligned_4k(512));
        assert!(r.parts[1].uuid.matches("deadbeef-02"));
        assert!(r.parts[1].uuid.matches("DEADBEEF-02"));
        assert!(!r.parts[1].uuid.matches("deadbeef-01"));
    }

    #[test]
    fn mbr_entry_past_end_is_dropped() {
        let mut d = Disk::new(SS, 10_000);
        mbr_entry(&mut d, 0, 0, 0x83, 2048, 20_000);
        assert!(d.scan().parts.is_empty());
    }

    #[test]
    fn superfloppy_fat_is_not_an_mbr() {
        // A FAT boot sector: 55 AA, but BPB/boot-code bytes in the entry area.
        let mut d = Disk::new(SS, 4096);
        let s0 = d.at(0);
        for (i, b) in s0[446..510].iter_mut().enumerate() { *b = (i as u8).wrapping_mul(37) | 1; }
        s0[510] = 0x55;
        s0[511] = 0xAA;
        assert_eq!(d.scan().table, Table::None);
    }

    #[test]
    fn misaligned_partition_is_flagged() {
        let mut d = Disk::new(SS, 1 << 16);
        mbr_entry(&mut d, 0, 0, 0x83, 63, 10_000); // classic CHS-era start
        let r = d.scan();
        assert_eq!(r.parts.len(), 1);
        assert!(!r.parts[0].aligned_4k(512));
        // On a 4Kn disk every LBA is 4096-aligned.
        assert!(r.parts[0].aligned_4k(4096));
    }

    #[test]
    fn gpt_primary() {
        let u1 = [0xA1u8; 16];
        let mut u2 = [0u8; 16];
        for (i, b) in u2.iter_mut().enumerate() { *b = i as u8; }
        let d = make_gpt(SS, 1 << 16, &[
            (ESP_GUID, u1, 2048, 4095),
            ([0; 16], [0; 16], 0, 0), // hole at slot 2
            (LINUX_FS_GUID, u2, 4096, 60_000),
        ]);
        let r = d.scan();
        assert_eq!(r.table, Table::Gpt { disk_guid: [0x11; 16], backup: false });
        assert_eq!(r.parts.len(), 2);
        assert_eq!(r.parts[0].pno, 1);
        assert_eq!(r.parts[0].num_lbas, 2048);
        assert_eq!(r.parts[1].pno, 3);
        assert_eq!(r.parts[1].start_lba, 4096);
        assert!(r.parts[1].ptype.is_linux_fs());
        assert!(!r.parts[0].ptype.is_linux_fs());
        // Mixed-endian text form of bytes 00 01 02 .. 0f.
        assert!(r.parts[1].uuid.matches("03020100-0504-0706-0809-0a0b0c0d0e0f"));
        assert!(r.parts[1].uuid.matches("03020100-0504-0706-0809-0A0B0C0D0E0F"));
    }

    #[test]
    fn gpt_linux_guid_text() {
        let mut buf = [0u8; 36];
        let n = PartUuid::Gpt(LINUX_FS_GUID).format(&mut buf);
        assert_eq!(&buf[..n], b"0fc63daf-8483-4772-8e79-3d69d8477de4");
    }

    #[test]
    fn gpt_backup_when_primary_header_corrupt() {
        let mut d = make_gpt(SS, 1 << 16, &[(LINUX_FS_GUID, [7; 16], 2048, 30_000)]);
        d.at(1)[40] ^= 0xFF; // breaks the header CRC
        let r = d.scan();
        assert_eq!(r.table, Table::Gpt { disk_guid: [0x11; 16], backup: true });
        assert_eq!(r.parts.len(), 1);
        assert_eq!(r.parts[0].start_lba, 2048);
    }

    #[test]
    fn gpt_backup_when_primary_entries_corrupt() {
        let mut d = make_gpt(SS, 1 << 16, &[(LINUX_FS_GUID, [7; 16], 2048, 30_000)]);
        d.at(2)[40] ^= 0xFF; // primary entry array no longer matches its CRC
        let r = d.scan();
        assert_eq!(r.table, Table::Gpt { disk_guid: [0x11; 16], backup: true });
        assert_eq!(r.parts[0].num_lbas, 30_000 - 2048 + 1);
    }

    #[test]
    fn gpt_both_headers_bad_means_no_table() {
        let mut d = make_gpt(SS, 1 << 16, &[(LINUX_FS_GUID, [7; 16], 2048, 30_000)]);
        d.at(1)[0] = b'X';
        let n = d.nsec();
        d.at(n - 1)[0] = b'X';
        let r = d.scan();
        assert_eq!(r.table, Table::None);
        assert!(r.parts.is_empty()); // the 0xEE entry is never a partition
    }

    #[test]
    fn gpt_on_4k_sector_disk() {
        let d = make_gpt(4096, 1 << 14, &[(LINUX_FS_GUID, [9; 16], 256, 10_000)]);
        let r = d.scan();
        assert!(matches!(r.table, Table::Gpt { backup: false, .. }));
        assert_eq!(r.parts.len(), 1);
        assert!(r.parts[0].aligned_4k(4096));
    }

    #[test]
    fn gpt_header_wrong_my_lba_rejected() {
        // A primary header copied to LBA 1 that claims to live elsewhere.
        let mut d = make_gpt(SS, 1 << 16, &[(LINUX_FS_GUID, [7; 16], 2048, 30_000)]);
        let n = d.nsec();
        write_gpt_copy(&mut d, 1, n - 1, 2, &[(LINUX_FS_GUID, [7; 16], 2048, 30_000)]);
        // Re-stamp my_lba = 5 with a valid CRC.
        let h = d.at(1);
        h[24..32].copy_from_slice(&5u64.to_le_bytes());
        h[16..20].fill(0);
        let c = crc32(&h[..92]);
        h[16..20].copy_from_slice(&c.to_le_bytes());
        let r = d.scan();
        assert!(matches!(r.table, Table::Gpt { backup: true, .. }));
    }
}
