//! Host tests for the kernel's i915 hardware-independent module. The module
//! under test is included verbatim, so these exercise exactly the code the
//! kernel runs.

extern crate alloc;

#[path = "../../src/i915/hw.rs"]
pub mod hw;

#[path = "../../src/i915/uapi_abi.rs"]
pub mod uapi_abi;

#[path = "../../src/i915/softcs.rs"]
pub mod softcs;

#[path = "../../src/i915/vmbind.rs"]
pub mod vmbind;

#[path = "../../src/i915/wa.rs"]
pub mod wa;

#[path = "../../src/i915/gen9_renderstate.rs"]
pub mod gen9_renderstate;

#[cfg(test)]
mod g3_tests {
    use super::softcs::*;
    use super::vmbind::*;
    use super::uapi_abi::*;
    use std::collections::HashMap;

    struct Mem { m: HashMap<u64, u32>, mapped: Vec<(u64, u64)> }
    impl Mem {
        fn new(mapped: &[(u64, u64)]) -> Self { Mem { m: HashMap::new(), mapped: mapped.to_vec() } }
        fn ok(&self, va: u64) -> bool { self.mapped.iter().any(|&(s, l)| va >= s && va < s + l) }
        fn put(&mut self, va: u64, dws: &[u32]) { for (i, &d) in dws.iter().enumerate() { self.m.insert(va + 4 * i as u64, d); } }
    }
    impl GpuMem for Mem {
        fn read32(&self, va: u64) -> Option<u32> { if self.ok(va) { Some(*self.m.get(&va).unwrap_or(&0)) } else { None } }
        fn write32(&mut self, va: u64, v: u32) -> bool { if self.ok(va) { self.m.insert(va, v); true } else { false } }
        fn timestamp(&self) -> u64 { 0x1_2345_6789 }
    }
    const BBE: u32 = 0x0A << 23;
    fn sdi(va: u64, v: u32) -> [u32; 4] { [(0x20 << 23) | 2, va as u32, (va >> 32) as u32, v] }

    #[test]
    fn cmd_lengths() {
        assert_eq!(cmd_len(0), Some(1));                    // MI_NOOP
        assert_eq!(cmd_len(BBE), Some(1));
        assert_eq!(cmd_len((0x20 << 23) | 2), Some(4));     // SDI dword
        assert_eq!(cmd_len((0x22 << 23) | 0x81), Some(0x83)); // LRI, 8-bit length
        assert_eq!(cmd_len((0x31 << 23) | 1), Some(3));     // BBS gen8
        assert_eq!(cmd_len(0x7A00_0004), Some(6));          // PIPE_CONTROL
        assert_eq!(cmd_len(0x6904_0303), Some(1));          // PIPELINE_SELECT
        assert_eq!(cmd_len(0x5450_0005), Some(7));          // XY_COLOR_BLT
        assert_eq!(cmd_len(0x2000_0000), None);             // type 1: reserved
    }

    #[test]
    fn softcs_stores_and_chains() {
        let mut m = Mem::new(&[(0x10_0000, 0x2000), (0x8000_0000_0000 - 0x1000, 0x1000)]);
        let hi = 0x8000_0000_0000u64 - 0x1000; // top of the 47-bit range
        // First-level batch: store, call a second-level batch, store, chain.
        let mut b = vec![];
        b.extend_from_slice(&sdi(hi, 0xC0FFEE));
        b.extend_from_slice(&[(0x31 << 23) | (1 << 22) | 1, 0x10_1000, 0]); // 2nd level
        b.extend_from_slice(&sdi(hi + 4, 2));
        b.extend_from_slice(&[(0x31 << 23) | 1, 0x10_1800, 0]);              // chain
        m.put(0x10_0000, &b);
        let mut c = vec![0u32];                                               // NOOP
        c.extend_from_slice(&sdi(hi + 8, 3));
        c.push(BBE);                                                          // returns
        m.put(0x10_1000, &c);
        let mut d = vec![0x7A00_0004, 1 << 14, (hi + 16) as u32, (hi >> 32) as u32, 0x1111, 0x2222];
        d.extend_from_slice(&[0x7A00_0004, 3 << 14, (hi + 24) as u32, (hi >> 32) as u32, 0, 0]);
        d.extend_from_slice(&[(0x24 << 23) | 2, 0x2358, (hi + 32) as u32, (hi >> 32) as u32]);
        d.push(BBE);
        m.put(0x10_1800, &d);
        let r = run(&mut m, 0x10_0000, 1000);
        let Outcome::Done(st) = r else { panic!("{:?}", r) };
        assert_eq!(st.chains, 2);
        assert_eq!(st.stores, 6);
        let g = |va: u64| m.read32(va).unwrap();
        assert_eq!((g(hi), g(hi + 4), g(hi + 8)), (0xC0FFEE, 2, 3));
        assert_eq!((g(hi + 16), g(hi + 20)), (0x1111, 0x2222));
        assert_eq!((g(hi + 24), g(hi + 28)), (0x2345_6789, 1));
        assert_eq!(g(hi + 32), 0x2345_6789);
    }

    #[test]
    fn softcs_faults_and_runaways() {
        let mut m = Mem::new(&[(0x1000, 0x1000)]);
        m.put(0x1000, &sdi(0x9000, 1));
        assert!(matches!(run(&mut m, 0x1000, 100), Outcome::Fault { va: 0x9000, .. }));
        assert!(matches!(run(&mut m, 0x5000, 100), Outcome::Fault { va: 0x5000, .. }));
        m.put(0x1000, &[0x2000_0000]);
        assert!(matches!(run(&mut m, 0x1000, 100), Outcome::BadCommand { .. }));
        m.put(0x1000, &[(0x31 << 23) | 1, 0x1000, 0]); // jumps to itself
        assert!(matches!(run(&mut m, 0x1000, 100), Outcome::Runaway(_)));
    }

    #[test]
    fn vm_bind_rules() {
        let mut v = VmMap::new();
        let p = v.plan(1, 0x10000, 0x4000);
        assert_eq!(p, Plan { unbind: vec![], bind: true });
        v.commit(1, 0x10000, 0x4000, &p);
        // Same place: nothing to do.
        assert!(!v.plan(1, 0x10000, 0x4000).bind);
        // Another object overlapping the tail evicts object 1.
        let p = v.plan(2, 0x12000, 0x4000);
        assert_eq!(p.unbind, vec![(0x10000, 0x4000)]);
        v.commit(2, 0x12000, 0x4000, &p);
        assert_eq!(v.find(1), None);
        // Object 2 moves: its old range goes.
        let p = v.plan(2, 0x40000, 0x4000);
        assert_eq!(p.unbind, vec![(0x12000, 0x4000)]);
        v.commit(2, 0x40000, 0x4000, &p);
        assert_eq!(v.find(2), Some((0x40000, 0x4000)));
        assert_eq!(v.covering(0x43FFF).map(|x| x.1.obj), Some(2));
        assert_eq!(v.covering(0x44000), None);
        // Adjacent, not overlapping: no eviction.
        let p = v.plan(3, 0x44000, 0x1000);
        assert!(p.unbind.is_empty());
        v.commit(3, 0x44000, 0x1000, &p);
        assert_eq!(v.remove_obj(2), Some((0x40000, 0x4000)));
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn execbuf_extensions() {
        let legacy = |r: u64| r == EXEC_DEFAULT || r == EXEC_RENDER;
        let mut e = Execbuffer2 { buffer_count: 1, flags: EXEC_USE_EXTENSIONS, cliprects_ptr: 0x1000, ..Default::default() };
        assert!(check_execbuf(&e, None, &legacy).unwrap().extensions);
        e.flags |= EXEC_FENCE_ARRAY;
        assert_eq!(check_execbuf(&e, None, &legacy), Err(EINVAL));
        e.flags = EXEC_USE_EXTENSIONS;
        e.num_cliprects = 1;
        assert_eq!(check_execbuf(&e, None, &legacy), Err(EINVAL));
    }
}

#[cfg(test)]
mod tests {
    use super::hw::*;
    use std::collections::HashMap;

    // ── identification ──────────────────────────────────────────────────────

    #[test]
    fn identify_target_and_family() {
        let p = identify(0x5917).expect("UHD 620 (KBL-R) must be known");
        assert_eq!(p.family, Family::Kbl);
        assert_eq!(p.gt, 2);
        assert!(p.is_target_laptop_gpu());
        assert!(p.has_llc());
        assert!(p.family.is_gen95());
        assert_eq!(identify(0x1916).unwrap().family, Family::Skl);
        assert_eq!(identify(0x3EA0).unwrap().family, Family::Cfl);
        assert_eq!(identify(0x9B41).unwrap().family, Family::Cml);
        assert!(identify(0x5A85).unwrap().family.is_lp());
        assert!(!identify(0x5A85).unwrap().has_llc());
        assert_eq!(identify(0x29C0), None); // QEMU q35 host bridge
        assert_eq!(identify(0x1234), None);
        assert_eq!(pch_name(0x9D4E), "Sunrise Point-LP (SPT-LP)");
        assert_eq!(pch_name(0x9D84), "Cannon Point-LP (CNP-LP)");
        assert_eq!(pch_name(0x2918), "unknown PCH"); // ICH9 (QEMU)
    }

    #[test]
    fn device_table_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for d in 0u32..=0xFFFF {
            if identify(d as u16).is_some() { assert!(seen.insert(d)); }
        }
        assert!(seen.len() > 80);
    }

    // ── GMCH / stolen ───────────────────────────────────────────────────────

    #[test]
    fn ggc_decode() {
        // GMS=0x02 (64 MiB stolen), GGMS=3 (8 MiB of PTEs = 4 GiB GGTT), locked.
        let g = decode_ggc(0x02C1);
        assert!(g.locked);
        assert!(!g.vga_disabled);
        assert_eq!(g.gtt_table_bytes, 8 << 20);
        assert_eq!(g.ggtt_entries, 1 << 20);
        assert_eq!(g.ggtt_size, 4 << 30);
        assert_eq!(g.stolen_size, 64 << 20);
        // GGMS=2: 4 MiB of PTEs, 2 GiB GGTT. GMS 0xF0 = 4 MiB, 0xF1 = 8 MiB.
        let g = decode_ggc(0xF082);
        assert!(g.vga_disabled);
        assert_eq!(g.ggtt_size, 2 << 30);
        assert_eq!(g.stolen_size, 4 << 20);
        assert_eq!(decode_ggc(0xF100).stolen_size, 8 << 20);
        assert_eq!(decode_ggc(0x0000).ggtt_size, 0);
        assert_eq!(decode_ggc(0x0500).stolen_size, 160 << 20);
    }

    #[test]
    fn bdsm_and_reserved() {
        assert_eq!(decode_bdsm(0x7B00_0001), (0x7B00_0000, true));
        assert_eq!(decode_bdsm(0x7B0F_FFFE), (0x7B00_0000, false));
        let top = 0x7B00_0000 + (64 << 20);
        assert_eq!(decode_stolen_reserved(0x7EF0_0001, top), Some((0x7EF0_0000, top - 0x7EF0_0000)));
        assert_eq!(decode_stolen_reserved(0x7EF0_0000, top), None); // disabled
        assert_eq!(decode_stolen_reserved(0x0000_0001, top), None);
    }

    #[test]
    fn forcewake_masks_and_ranges() {
        assert_eq!(masked_en(reg::FORCEWAKE_KERNEL), 0x0001_0001);
        assert_eq!(masked_dis(reg::FORCEWAKE_KERNEL), 0x0001_0000);
        assert_eq!(gen9_fw_domains(0x2034), 2); // RCS ring head: render
        assert_eq!(gen9_fw_domains(0x22034), 1); // BCS: GT
        assert_eq!(gen9_fw_domains(0x12034), 4); // VCS: media
        assert_eq!(gen9_fw_domains(0x1a034), 4); // VECS: media
        assert_eq!(gen9_fw_domains(0x9120), 1); // FUSE2: GT
        assert_eq!(gen9_fw_domains(0x9400), 6); // render | media
        assert_eq!(gen9_fw_domains(0xa188), 1);
        assert_eq!(gen9_fw_domains(0x1000), 0); // uncore
        assert_eq!(gen9_fw_domains(0x70180), 0); // display
    }

    // ── SSEU ────────────────────────────────────────────────────────────────

    #[test]
    fn sseu_kbl_gt2() {
        // 1 slice; subslice 3 fused off (GT2 = 3 x 8 EU).
        let fuse2 = (1 << 25) | (0x8 << 20);
        let s = gen9_sseu(fuse2, [0, 0, 0], false);
        assert_eq!(s.slice_mask, 1);
        assert_eq!(s.subslice_mask, 0x7);
        assert_eq!(s.eu_total, 24);
        assert_eq!(s.eu_per_subslice, 8);
        assert_eq!(s.subslices_total(), 3);
        assert_eq!(s.subslice_7eu, [0, 0, 0]);
        // One EU of subslice 1 fused off: 23 EUs, 7-EU subslice recorded.
        let s = gen9_sseu(fuse2, [0x0000_0100, 0, 0], false);
        assert_eq!(s.eu_total, 23);
        assert_eq!(s.eu_mask[0][1], 0xFE);
        assert_eq!(s.subslice_7eu[0], 0b10);
        assert_eq!(s.eu_per_subslice, 8);
        // Disabled slices are ignored even if their EU fuses are set.
        let s = gen9_sseu(fuse2, [0, 0xFFFF_FFFF, 0], false);
        assert_eq!(s.eu_total, 24);
    }

    #[test]
    fn topology_blob_layout() {
        let s = gen9_sseu((1 << 25) | (0x8 << 20), [0x0000_0100, 0, 0], false);
        let b = topology_blob(&s);
        let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        assert_eq!(u16at(2), 3); // max_slices
        assert_eq!(u16at(4), 4); // max_subslices
        assert_eq!(u16at(6), 8); // max_eus_per_subslice
        let ss_off = u16at(8) as usize;
        let ss_stride = u16at(10) as usize;
        let eu_off = u16at(12) as usize;
        let eu_stride = u16at(14) as usize;
        assert_eq!((ss_off, ss_stride, eu_off, eu_stride), (1, 1, 4, 1));
        let d = &b[16..];
        assert_eq!(d.len(), 4 + 3 * 4);
        assert_eq!(d[0], 0b1);
        assert_eq!(d[ss_off], 0b111);
        assert_eq!(d[ss_off + 1], 0); // slice 1 disabled
        assert_eq!(d[eu_off], 0xFF);
        assert_eq!(d[eu_off + 1], 0xFE);
        assert_eq!(d[eu_off + 3], 0); // subslice 3 fused off
    }

    // ── clocks ──────────────────────────────────────────────────────────────

    #[test]
    fn timestamp_and_freq() {
        assert_eq!(gen9_cs_timestamp_hz(2 << 1, 0, false), 12_000_000);
        assert_eq!(gen9_cs_timestamp_hz(3 << 1, 0, false), 24_000_000);
        assert_eq!(gen9_cs_timestamp_hz(0, 0, false), 3_000_000);
        assert_eq!(gen9_cs_timestamp_hz(3 << 1, 0, true), 19_200_000);
        // Divide logic: divider 23 -> 24 MHz, denominator 0 -> +1 MHz.
        assert_eq!(gen9_cs_timestamp_hz(1, 23, false), 25_000_000);
        // KBL-R UHD 620: RP0 1100 MHz (22), RP1 300 MHz (6), RPn 300 MHz (6).
        assert_eq!(decode_rp_state_cap(0x0006_0616), (1100, 300, 300));
        assert_eq!(decode_cagf_mhz(18 << 23), 300);
    }

    // ── PTEs and PPAT ───────────────────────────────────────────────────────

    #[test]
    fn pte_encoding() {
        assert_eq!(ggtt_pte(0x1_2345_6000), 0x1_2345_6001);
        assert_eq!(ggtt_pte_phys(0x1_2345_6001), Some(0x1_2345_6000));
        assert_eq!(ggtt_pte_phys(0x1_2345_6000), None);
        let p = ppgtt_pte(0x8000_0000, Cache::Cached, false);
        assert_eq!(p, 0x8000_0083);
        assert_eq!(pte_pat_index(p), 4); // Linux PPAT_CACHED = _PAGE_PAT
        assert_eq!(pte_pat_index(ppgtt_pte(0, Cache::CachedPde, false)), 0);
        assert_eq!(pte_pat_index(ppgtt_pte(0, Cache::Display, false)), 2);
        assert_eq!(pte_pat_index(ppgtt_pte(0, Cache::Uncached, false)), 3);
        assert_eq!(ppgtt_pte(0x1000, Cache::Cached, true), 0x1081);
        assert_eq!(ppgtt_pde(0x5000), 0x5003);
        assert_eq!(gen9_private_ppat(), 0x3B2B_1B0B_0003_0907);
    }

    struct MapStore { pages: HashMap<u64, Vec<u64>>, next: u64, allocated: usize, freed: usize, limit: usize }
    impl MapStore {
        fn new() -> Self { MapStore { pages: HashMap::new(), next: 0x10_0000, allocated: 0, freed: 0, limit: usize::MAX } }
    }
    impl PtStore for MapStore {
        fn alloc_page(&mut self) -> Option<u64> {
            if self.allocated - self.freed >= self.limit { return None; }
            let p = self.next;
            self.next += 4096;
            self.pages.insert(p, vec![0; 512]);
            self.allocated += 1;
            Some(p)
        }
        fn free_page(&mut self, phys: u64) {
            assert!(self.pages.remove(&phys).is_some(), "double free of {phys:#x}");
            self.freed += 1;
        }
        fn read(&self, t: u64, i: usize) -> u64 { self.pages[&t][i] }
        fn write(&mut self, t: u64, i: usize, v: u64) { self.pages.get_mut(&t).unwrap()[i] = v; }
    }

    #[test]
    fn ppgtt_map_translate_unmap() {
        let mut st = MapStore::new();
        let mut pt = Ppgtt::new(&mut st).unwrap();
        let scratch = pt.scratch[0];
        // Anything unmapped resolves to the scratch page.
        assert_eq!(pt.translate(&st, 0x7FFF_FFFF_F000), Some(scratch));
        assert_eq!(pt.translate(&st, 0x1234), Some(scratch + 0x234));
        assert_eq!(ppgtt_indices((1 << 39) | (1 << 30) | (1 << 21) | (0x103 << 12)), [1, 1, 1, 0x103]);
        pt.map(&mut st, 0x1_0000_0000, 0x4000_0000, Cache::Cached, false).unwrap();
        assert_eq!(pt.translate(&st, 0x1_0000_0abc), Some(0x4000_0abc));
        assert_eq!(pt.table_pages(), 3); // PDP, PD, PT
        // Neighbours in the same PT reuse it; a far address builds a new chain.
        pt.map(&mut st, 0x1_0000_1000, 0x4000_5000, Cache::Uncached, false).unwrap();
        assert_eq!(pt.table_pages(), 3);
        assert_eq!(pte_pat_index(pt.lookup(&st, 0x1_0000_1000)), 3);
        pt.map(&mut st, 0xFFFF_0000_0000, 0x5000_0000, Cache::Cached, false).unwrap();
        assert_eq!(pt.table_pages(), 6);
        assert_eq!(pt.translate(&st, 0xFFFF_0000_0010), Some(0x5000_0010));
        // A neighbouring page of a mapped one is still scratch.
        assert_eq!(pt.translate(&st, 0x1_0000_2000), Some(scratch));
        pt.unmap(&mut st, 0x1_0000_0000);
        assert_eq!(pt.translate(&st, 0x1_0000_0000), Some(scratch));
        assert_eq!(pt.translate(&st, 0x1_0000_1000), Some(0x4000_5000));
        // Remap the same VA to another page.
        pt.map(&mut st, 0x1_0000_1000, 0x6000_0000, Cache::Cached, false).unwrap();
        assert_eq!(pt.translate(&st, 0x1_0000_1000), Some(0x6000_0000));
        assert_eq!(pt.map(&mut st, 0x1001, 0, Cache::Cached, false), Err(PtError::Misaligned));
        assert_eq!(pt.map(&mut st, 1 << 48, 0, Cache::Cached, false), Err(PtError::OutOfRange));
        assert_eq!(pt.map(&mut st, 0, 1 << 39, Cache::Cached, false), Err(PtError::OutOfRange));
        pt.destroy(&mut st);
        assert_eq!(st.allocated, st.freed);
        assert!(st.pages.is_empty());
    }

    #[test]
    fn ppgtt_range_and_oom() {
        let mut st = MapStore::new();
        let mut pt = Ppgtt::new(&mut st).unwrap();
        // 2 MiB + 8 KiB range crosses a PT boundary.
        pt.map_range(&mut st, 0x20_0000 - 0x1000, 0x9000_0000, 0x20_0000 + 0x2000, Cache::Cached).unwrap();
        for o in (0..0x20_2000u64).step_by(4096) {
            assert_eq!(pt.translate(&st, 0x1F_F000 + o), Some(0x9000_0000 + o));
        }
        assert_eq!(pt.table_pages(), 5); // PDP, PD, 3 x PT (the range touches three 2 MiB slots)
        st.limit = st.allocated - st.freed; // no more pages
        assert_eq!(pt.map(&mut st, 0x7000_0000_0000, 0, Cache::Cached, false), Err(PtError::NoMemory));
        pt.destroy(&mut st);
        assert!(st.pages.is_empty());
    }

    // ── commands ────────────────────────────────────────────────────────────

    /// Length a command header claims: (low 8 bits) + 2 dwords.
    fn claimed_len(h: u32) -> usize { (h & 0xFF) as usize + 2 }

    #[test]
    fn command_builders() {
        let mut cs = Vec::new();
        cmd::store_dword_ggtt(&mut cs, 0x1_0000_0100, 0xdead_beef);
        assert_eq!(cs, vec![0x1040_0002, 0x100, 1, 0xdead_beef]);
        assert_eq!(claimed_len(cs[0]), 4);
        cs.clear();
        cmd::flush_dw_store_ggtt(&mut cs, 0x100, 7, false);
        assert_eq!(cs, vec![(0x26 << 23) | 2 | (1 << 14), 0x104, 0, 7]);
        assert_eq!(claimed_len(cs[0]), 4);
        cs.clear();
        cmd::xy_color_blt(&mut cs, 0x1_2000_0000, 7680, false, 0, 0, 1920, 64, 0x00FF_0000);
        assert_eq!(cs.len(), 7);
        assert_eq!(claimed_len(cs[0]), 7);
        assert_eq!(cs[0], 0x5430_0005);
        assert_eq!(cs[1], 0x03F0_0000 | 7680);
        assert_eq!(cs[2], 0);
        assert_eq!(cs[3], (64 << 16) | 1920);
        assert_eq!((cs[4], cs[5], cs[6]), (0x2000_0000, 1, 0x00FF_0000));
        cs.clear();
        cmd::xy_color_blt(&mut cs, 0, 8192, true, 0, 0, 1, 1, 0);
        assert_ne!(cs[0] & cmd::XY_BLT_DST_TILED, 0);
        assert_eq!(cs[1] & 0xFFFF, 2048);
        cs.clear();
        cmd::xy_src_copy_blt(&mut cs, 0x1000, 256, 1, 2, 0x2000, 512, 3, 4, 10, 20);
        assert_eq!(cs.len(), 10);
        assert_eq!(claimed_len(cs[0]), 10);
        assert_eq!(cs[3], (22 << 16) | 11);
        cs.clear();
        cmd::batch_start(&mut cs, 0x1_0000_1000, true);
        assert_eq!(cs[1], (0x31 << 23) | 1 | (1 << 8));
        assert_eq!(claimed_len(cs[1]), 3);
        assert_eq!(cmd::lri_header(1), 0x1100_0001);
        assert_eq!(cmd::mi_opcode(cmd::MI_BATCH_BUFFER_END), 0x0A);
        let mut cs = vec![1u32, 2, 3];
        cmd::pad_qword(&mut cs);
        assert_eq!(cs.len(), 4);
    }

    #[test]
    fn render_pipe_controls() {
        assert_eq!(cmd::PIPE_CONTROL6, 0x7A00_0004);
        assert_eq!(claimed_len(cmd::PIPE_CONTROL6), 6);
        let mut cs = Vec::new();
        cmd::rcs_invalidate(&mut cs, 0xFF00_00D0);
        assert_eq!(cs.len(), 12);
        assert_eq!(&cs[..6], &[cmd::PIPE_CONTROL6, 0, 0, 0, 0, 0]); // gen9 null PIPE_CONTROL
        assert_ne!(cs[7] & cmd::PC_TLB_INVALIDATE, 0);
        assert_ne!(cs[7] & cmd::PC_QW_WRITE, 0); // TLB invalidate needs a post-sync op
        assert_eq!(cs[8], 0xFF00_00D0);
        cs.clear();
        cmd::rcs_breadcrumb(&mut cs, 0xFF00_0100, 42);
        assert_eq!(cs.len(), 12);
        assert_eq!(cs[7], cmd::PC_FLUSH_ENABLE | cmd::PC_CS_STALL | cmd::PC_GLOBAL_GTT_IVB | cmd::PC_QW_WRITE);
        assert_eq!((cs[8], cs[10]), (0xFF00_0100, 42));
    }

    #[test]
    fn ring_math() {
        assert_eq!(ring_ctl(16 * 1024), 0x3001);
        assert_eq!(ring_ctl(4096), 0x0001);
        assert_eq!(ring_space(0, 0, 16384), 16384 - 8);
        assert_eq!(ring_space(0, 1024, 16384), 16384 - 1024 - 8);
        assert_eq!(ring_space(1024, 0, 16384), 1024 - 8);
    }

    // ── LRC ─────────────────────────────────────────────────────────────────

    #[test]
    fn lrc_layout_matches_linux_ctx_indices() {
        let base = reg::BLT_RING_BASE;
        let regs = lrc_registers(GEN9_XCS_OFFSETS, base);
        assert_eq!(regs.len(), 14 + 9 + 1 + 44);
        let at = |idx: usize| regs.iter().find(|r| r.0 == idx).map(|r| r.1);
        assert_eq!(at(ctx::CONTEXT_CONTROL), Some(base + 0x244));
        assert_eq!(at(ctx::RING_HEAD), Some(base + 0x34));
        assert_eq!(at(ctx::RING_TAIL), Some(base + 0x30));
        assert_eq!(at(ctx::RING_START), Some(base + 0x38));
        assert_eq!(at(ctx::RING_CTL), Some(base + 0x3c));
        assert_eq!(at(ctx::BB_STATE), Some(base + 0x110));
        assert_eq!(at(ctx::TIMESTAMP), Some(base + 0x3a8));
        assert_eq!(at(ctx::PDP3_UDW), Some(base + 0x28c));
        assert_eq!(at(ctx::PDP0_UDW), Some(base + 0x274));
        assert_eq!(at(ctx::PDP0_LDW), Some(base + 0x270));
        assert_eq!(at(ctx::GEN9_RING_MI_MODE + 1), Some(base + 0x9c));
        assert_eq!(at(ctx::GEN9_BB_OFFSET + 1), Some(base + 0x158));
        assert_eq!(at(0x43), Some(base + 0x200)); // xcs: RING_MODE? (0x200 slot)
        // The 44-register block ends with GPR-ish 0x68 at its last slot.
        assert_eq!(regs.last().unwrap().1, base + 0x68);
        assert!(regs.iter().any(|r| r.1 == base + 0x67c));
        let rcs = lrc_registers(GEN9_RCS_OFFSETS, reg::RENDER_RING_BASE);
        assert_eq!(rcs.len(), 68);
        assert_eq!(rcs.iter().find(|r| r.0 == ctx::R_PWR_CLK_STATE).unwrap().1, 0x20c8);
        // Same layout as xcs except the single LRI.
        for (a, b) in rcs.iter().zip(regs.iter()) {
            assert_eq!(a.0, b.0);
        }
    }

    #[test]
    fn lrc_init_values() {
        let mut regs = vec![0xAAAA_AAAAu32; 1024];
        let p = LrcInit {
            class: EngineClass::Copy, mmio_base: reg::BLT_RING_BASE, ring_ggtt: 0xFF00_0000,
            ring_size: 16384, head: 0, tail: 0x40, pml4: 0x1_2345_6000, inhibit: true, rpcs: 0,
        };
        lrc_init_regs(&mut regs, &p);
        assert_eq!(regs[0], 0); // NOP(1), cleared by inhibit
        assert_eq!(regs[1], cmd::lri_header(14) | cmd::MI_LRI_FORCE_POSTED);
        assert_eq!(regs[ctx::CONTEXT_CONTROL], 0x000F_0009);
        assert_eq!(regs[ctx::RING_START], 0xFF00_0000);
        assert_eq!(regs[ctx::RING_TAIL], 0x40);
        assert_eq!(regs[ctx::RING_HEAD], 0);
        assert_eq!(regs[ctx::RING_CTL], 0x3001);
        assert_eq!(regs[ctx::PDP0_UDW], 1);
        assert_eq!(regs[ctx::PDP0_LDW], 0x2345_6000);
        assert_eq!(regs[ctx::GEN9_RING_MI_MODE + 1], reg::STOP_RING << 16);
        // MI_BATCH_BUFFER_END closes the image right after the last pair.
        let last = lrc_registers(GEN9_XCS_OFFSETS, reg::BLT_RING_BASE).last().unwrap().0;
        assert_eq!(regs[last + 1], cmd::MI_BATCH_BUFFER_END);
        assert!(regs[last + 2..].iter().all(|&r| r == 0));
        // Without inhibit nothing outside the touched slots changes.
        let mut regs2 = vec![0x5555_5555u32; 1024];
        lrc_init_regs(&mut regs2, &LrcInit { inhibit: false, ..p });
        assert_eq!(regs2[ctx::CONTEXT_CONTROL], 0x000F_0008);
        assert_eq!(regs2[1023], 0x5555_5555);
    }

    #[test]
    fn descriptor_and_elsp() {
        let d = ctx_descriptor(0x1000_0000, 5, true, false);
        assert_eq!(d, 0x5_1000_0119);
        assert_eq!(ctx_descriptor(0x1000_0fff, 5, true, true), 0x5_1000_011D);
        assert_eq!(ctx_descriptor(0x2000, 0, false, false) & 0x18, 0x08);
        assert_eq!(ctx_descriptor(0x2000, 1 << 21, true, false) >> 32, 0);
        assert_eq!(elsp_writes(0x5_1000_0119, 0), [0, 0, 5, 0x1000_0119]);
    }

    #[test]
    fn csb_helpers() {
        assert_eq!(csb_ptr_reset(), 0xFFFF_0505);
        assert_eq!(csb_pending(5, 1), vec![0, 1]);
        assert_eq!(csb_pending(2, 2), Vec::<u32>::new());
        assert_eq!(csb_pending(0, 5), vec![1, 2, 3, 4, 5]);
        assert_eq!(csb_status_str(CSB_IDLE_ACTIVE), "idle->active");
        assert_eq!(csb_status_str(CSB_ACTIVE_IDLE | CSB_COMPLETE), "active->idle|complete");
        assert_eq!(csb_status_str(0), "none");
    }

    // ── hang detector ───────────────────────────────────────────────────────

    #[test]
    fn hang_check() {
        let ms = 1_000_000u64;
        let mut h = HangCheck::new(0, 1000 * ms, 100 * ms);
        assert_eq!(h.sample(1 * ms, false, 0x100, 0, 0), HangVerdict::Running);
        assert_eq!(h.sample(50 * ms, false, 0x100, 0, 0), HangVerdict::Running);
        assert_eq!(h.sample(90 * ms, false, 0x140, 0, 0), HangVerdict::Running); // progress
        assert_eq!(h.sample(180 * ms, false, 0x140, 0, 0), HangVerdict::Running);
        assert_eq!(h.sample(190 * ms, false, 0x140, 0, 0), HangVerdict::Hung); // 100 ms stall
        let mut h = HangCheck::new(0, 300 * ms, 100 * ms);
        let mut t = 0;
        let mut v = HangVerdict::Running;
        while v == HangVerdict::Running { t += 10 * ms; v = h.sample(t, false, t, 0, 0); }
        assert_eq!(v, HangVerdict::Timeout); // always moving, budget spent
        assert_eq!(t, 300 * ms);
        let mut h = HangCheck::new(0, 300 * ms, 100 * ms);
        assert_eq!(h.sample(1, false, 0, 0, 1), HangVerdict::Hung); // EIR
        assert_eq!(h.sample(1, true, 0, 0, 1), HangVerdict::Done);
    }

    // ── display decode ──────────────────────────────────────────────────────

    #[test]
    fn display_decode() {
        let ctl = 0x8400_0400; // enable, XRGB8888, X-tiled
        assert_eq!(plane_format_name(ctl), "XRGB8888");
        assert_eq!(plane_tiling(ctl), Tiling::X);
        assert_eq!(plane_tiling(0x8400_0000), Tiling::Linear);
        assert_eq!(plane_stride_bytes(120, Tiling::Linear), 7680);
        assert_eq!(plane_stride_bytes(15, Tiling::X), 7680);
        let f = decode_trans_ddi_func_ctl(0x8200_0006);
        assert!(f.enabled);
        assert_eq!((f.port, f.mode, f.bpc, f.lanes), ('A', "DP-SST", 8, 4));
        assert_eq!(f.edp_input, "pipe A (always on)");
        let f = decode_trans_ddi_func_ctl(0x8A10_5002);
        assert_eq!((f.port, f.bpc, f.lanes), ('A', 10, 2));
        assert_eq!(f.edp_input, "pipe B");
        assert_eq!(pipe_src((1919 << 16) | 1079), (1920, 1080));
        assert_eq!(timing_pair((2199 << 16) | 1919), (1920, 2200));
        assert_eq!(skl_power_wells(0), "none");
        assert_eq!(skl_power_wells((3 << 28) | (3 << 30)), "PW1=on PW2=on");
        assert_eq!(skl_power_wells(2 << 2), "DDI_A_E=req");
        assert_eq!(reg::pipe(reg::TRANSCONF_A, 1), 0x71008);
        assert_eq!(reg::plane(reg::PLANE_CTL_1_A, 0, 1), 0x70280);
        assert_eq!(reg::port(reg::DDI_BUF_CTL_A, 4), 0x64400);
    }

    // ── DMC ─────────────────────────────────────────────────────────────────

    fn put32(v: &mut Vec<u8>, o: usize, x: u32) {
        if v.len() < o + 4 { v.resize(o + 4, 0); }
        v[o..o + 4].copy_from_slice(&x.to_le_bytes());
    }

    /// A synthetic v1 DMC container shaped like kbl_dmc_ver1_04.bin.
    fn dmc_blob(mmio_addr: u32) -> Vec<u8> {
        let mut b = vec![0u8; 128];
        put32(&mut b, 0, 9); // module type DMC
        put32(&mut b, 4, 32); // header_len dwords
        put32(&mut b, 8, 0x10000);
        put32(&mut b, 0x14, 0x2018_0315);
        put32(&mut b, 0x58, 0x0001_0004);
        let p = b.len();
        let pkg_len = 16 + 20 * 12;
        b.resize(p + pkg_len, 0);
        b[p] = (pkg_len / 4) as u8;
        b[p + 1] = 1;
        put32(&mut b, p + 12, 2);
        // entry 0: stepping 'Z' (no match), entry 1: '*','*' at offset 0.
        b[p + 16 + 2] = b'Z'; b[p + 16 + 3] = b'*'; put32(&mut b, p + 16 + 4, 0x40);
        b[p + 28 + 2] = b'*'; b[p + 28 + 3] = b'*'; put32(&mut b, p + 28 + 4, 0);
        let h = b.len();
        b.resize(h + 128, 0);
        put32(&mut b, h, 0x4040_3E3E);
        b[h + 4] = 128;
        b[h + 5] = 1;
        put32(&mut b, h + 12, 4); // fw_size dwords
        put32(&mut b, h + 16, 0x0001_0004);
        put32(&mut b, h + 20, 2);
        put32(&mut b, h + 24, mmio_addr);
        put32(&mut b, h + 28, 0x8F004);
        put32(&mut b, h + 56, 0x18FE0);
        put32(&mut b, h + 60, 0x1C0F_0000);
        for i in 0..4 { put32(&mut b, h + 128 + 4 * i, 0x1111_1111 * (i as u32 + 1)); }
        b
    }

    #[test]
    fn dmc_parse() {
        let b = dmc_blob(0x8F074);
        let d = parse_dmc(&b, (b'B', b'0')).unwrap();
        assert_eq!(d.css_version, 0x0001_0004);
        assert_eq!(d.css_date, 0x2018_0315);
        assert_eq!(d.stepping, (b'*', b'*'));
        assert_eq!(d.dmc_header_ver, 1);
        assert_eq!(d.payload_dwords, 4);
        assert_eq!(d.payload_off, 128 + 256 + 128);
        assert_eq!(d.mmio, vec![(0x8F074, 0x18FE0), (0x8F004, 0x1C0F_0000)]);
        assert_eq!(u32::from_le_bytes(b[d.payload_off..d.payload_off + 4].try_into().unwrap()), 0x1111_1111);
        assert_eq!(parse_dmc(&dmc_blob(0x4000), (b'B', b'0')), Err(FwParseError::BadMmio));
        assert_eq!(parse_dmc(&b[..b.len() - 4], (b'B', b'0')), Err(FwParseError::Truncated));
        let mut bad = b.clone();
        put32(&mut bad, 4, 31);
        assert_eq!(parse_dmc(&bad, (b'B', b'0')), Err(FwParseError::BadCss));
    }

    /// The real linux-firmware blob, when ports/i915_fw/fetch.sh has cached it
    /// (I915_DMC_BLOB overrides the path). Skipped when absent.
    #[test]
    fn dmc_parse_real_kbl_blob() {
        let path = std::env::var("I915_DMC_BLOB").unwrap_or_else(|_|
            format!("{}/.cache/leandros/i915/kbl_dmc_ver1_04.bin", std::env::var("HOME").unwrap_or_default()));
        let b = match std::fs::read(&path) { Ok(b) => b, Err(_) => { eprintln!("skipped: no {path}"); return; } };
        let d = parse_dmc(&b, (b'B', b'0')).expect("kbl_dmc_ver1_04.bin must parse");
        assert_eq!(d.css_version, 0x0001_0004); // "ver1_04"
        assert_eq!(d.dmc_header_ver, 1);
        assert!(d.payload_dwords > 1000 && d.payload_off + d.payload_dwords * 4 <= b.len());
        assert!(!d.mmio.is_empty() && d.mmio.iter().all(|m| (0x8_0000..=0x8_FFFF).contains(&m.0)));
        eprintln!("kbl DMC: payload {} dwords at 0x{:x}, stepping {:?}, mmio {:x?}", d.payload_dwords, d.payload_off,
            (d.stepping.0 as char, d.stepping.1 as char), d.mmio);
    }

    // ── OpRegion / VBT ──────────────────────────────────────────────────────

    fn vbt_blob() -> Vec<u8> {
        let mut v = vec![0u8; 0x30];
        v[..20].copy_from_slice(b"$VBT KABYLAKE      d");
        v[20..22].copy_from_slice(&0x64u16.to_le_bytes());
        v[22..24].copy_from_slice(&0x30u16.to_le_bytes());
        put32(&mut v, 28, 0x30);
        let bdb = v.len();
        v.extend_from_slice(b"BIOS_DATA_BLOCK ");
        v.extend_from_slice(&221u16.to_le_bytes());
        v.extend_from_slice(&22u16.to_le_bytes()); // header size
        v.extend_from_slice(&0u16.to_le_bytes()); // bdb size (patched)
        for (id, data) in [(1u8, vec![0u8; 8]), (2, vec![0; 40]), (40, vec![3u8, 0, 0])] {
            v.push(id);
            v.extend_from_slice(&(data.len() as u16).to_le_bytes());
            v.extend_from_slice(&data);
        }
        let bdb_size = (v.len() - bdb) as u16;
        v[bdb + 20..bdb + 22].copy_from_slice(&bdb_size.to_le_bytes());
        let total = v.len() as u16;
        v[24..26].copy_from_slice(&total.to_le_bytes());
        v
    }

    #[test]
    fn vbt_parse() {
        let v = vbt_blob();
        let i = parse_vbt(&v).unwrap();
        assert_eq!(i.version, 0x64);
        assert_eq!(i.bdb_version, 221);
        assert_eq!(i.blocks, vec![(1, 8), (2, 40), (40, 3)]);
        assert_eq!(i.panel_type, Some(3));
        assert!(i.signature.starts_with("$VBT KABYLAKE"));
        assert_eq!(parse_vbt(&v[..20]), None);
        let mut bad = v.clone();
        bad[0x30] = b'X';
        assert_eq!(parse_vbt(&bad), None);
    }

    #[test]
    fn opregion_parse() {
        let mut o = vec![0u8; OPREGION_SIZE];
        o[..16].copy_from_slice(OPREGION_SIGNATURE);
        put32(&mut o, 16, 8);
        o[21] = 0; o[22] = 0; o[23] = 2; // version 2.0.0
        o[24..32].copy_from_slice(b"BIOS 1.2");
        put32(&mut o, 0x58, 0b10111); // ACPI, SWSCI, ASLE, ASLE_EXT
        let v = vbt_blob();
        o[OPREGION_VBT_OFFSET..OPREGION_VBT_OFFSET + v.len()].copy_from_slice(&v);
        let i = parse_opregion(&o, 0x7000_0000).unwrap();
        assert_eq!(i.version, (2, 0, 0));
        assert_eq!(i.bios_ver, "BIOS 1.2");
        assert_eq!(i.vbt, VbtLocation::Mailbox4(0x400, 0x1C00 - 0x400));
        // 2.1 with RVDA: relative to the OpRegion.
        o[22] = 1;
        o[OPREGION_ASLE_OFFSET + ASLE_RVDA..OPREGION_ASLE_OFFSET + ASLE_RVDA + 8].copy_from_slice(&0x2000u64.to_le_bytes());
        put32(&mut o, OPREGION_ASLE_OFFSET + ASLE_RVDS, 0x1800);
        assert_eq!(parse_opregion(&o, 0x7000_0000).unwrap().vbt, VbtLocation::Rvda(0x7000_2000, 0x1800));
        o[0] = b'X';
        assert_eq!(parse_opregion(&o, 0), None);
    }
}

#[cfg(test)]
mod uapi_tests {
    use super::hw::gen9_sseu;
    use super::uapi_abi::*;

    fn facts() -> Facts {
        Facts {
            device_id: 0x5917, revision: 7, has_llc: true,
            sseu: gen9_sseu((1 << 25) | (0x8 << 20), [0, 0, 0], false),
            cs_timestamp_hz: 12_000_000, engines: vec![(0, 0), (1, 0)],
            ram_total: 16 << 30, ram_free: 12 << 30, aperture: 256 << 20,
            exec_fence: false, exec_fence_array: true, timeline_fences: false,
        }
    }

    #[test]
    fn ioctl_numbers_match_the_header() {
        assert_eq!(ioctl::GETPARAM & 0xFF, NR_GETPARAM);
        assert_eq!(ioc_size(ioctl::GETPARAM), 16);
        assert_eq!(ioctl::QUERY & 0xFF, NR_QUERY);
        assert_eq!(ioc_size(ioctl::GEM_EXECBUFFER2), 64);
        assert_eq!(ioctl::GEM_EXECBUFFER2_WR & 0xFF, NR_GEM_EXECBUFFER2);
        assert_eq!(ioc_size(ioctl::GEM_CREATE_EXT), 24);
        assert_eq!(ioctl::GEM_MMAP_OFFSET & 0xFF, NR_GEM_MMAP_OFFSET);
        assert_eq!(ioc_size(ioctl::GEM_MMAP_OFFSET), 32);
        assert_eq!(ioc_size(ioctl::GEM_CONTEXT_GETPARAM), 24);
        assert_eq!(ioctl::GEM_VM_CREATE & 0xFF, NR_GEM_VM_CREATE);
        assert_eq!(ioctl::GEM_CREATE_EXT & 0xFF, NR_GEM_CREATE_EXT);
    }

    #[test]
    fn getparam_answers() {
        let f = facts();
        assert_eq!(getparam(param::CHIPSET_ID, &f), Ok(0x5917));
        assert_eq!(getparam(param::REVISION, &f), Ok(7));
        assert_eq!(getparam(param::EU_TOTAL, &f), Ok(24));
        assert_eq!(getparam(param::SUBSLICE_TOTAL, &f), Ok(3));
        assert_eq!(getparam(param::SLICE_MASK, &f), Ok(1));
        assert_eq!(getparam(param::SUBSLICE_MASK, &f), Ok(7));
        assert_eq!(getparam(param::CS_TIMESTAMP_FREQUENCY, &f), Ok(12_000_000));
        assert_eq!(getparam(param::MMAP_GTT_VERSION, &f), Ok(4)); // iris: has_mmap_offset
        assert_eq!(getparam(param::HAS_EXEC_SOFTPIN, &f), Ok(1));
        assert_eq!(getparam(param::HAS_CONTEXT_ISOLATION, &f), Ok(0b11));
        assert_eq!(getparam(param::HAS_LLC, &f), Ok(1));
        assert_eq!(getparam(param::HAS_BLT, &f), Ok(1));
        assert_eq!(getparam(param::HAS_BSD, &f), Ok(0));
        assert_eq!(getparam(param::HAS_EXEC_FENCE_ARRAY, &f), Ok(1));
        assert_eq!(getparam(param::HAS_EXEC_TIMELINE_FENCES, &f), Ok(0));
        assert_eq!(getparam(param::PXP_STATUS, &f), Err(ENODEV));
        assert_eq!(getparam(param::IRQ_ACTIVE, &f), Err(ENODEV));
        assert_eq!(getparam(999, &f), Err(EINVAL));
    }

    #[test]
    fn query_protocol() {
        let f = facts();
        let topo = query_blob(QUERY_TOPOLOGY_INFO, 0, &f);
        let n = topo.as_ref().unwrap().len() as i32;
        assert_eq!(query_length(0, &topo), (n, false)); // size probe
        assert_eq!(query_length(n - 1, &topo), (-EINVAL, false));
        assert_eq!(query_length(n + 100, &topo), (n, true));
        assert_eq!(query_length(0, &query_blob(QUERY_TOPOLOGY_INFO, 1, &f)), (-EINVAL, false));
        assert_eq!(query_length(0, &query_blob(QUERY_HWCONFIG_BLOB, 0, &f)), (-ENODEV, false));
        assert_eq!(query_length(0, &query_blob(42, 0, &f)), (-EINVAL, false));
    }

    #[test]
    fn engine_and_memory_blobs() {
        let b = engine_info_blob(&[(0, 0), (1, 0)]);
        assert_eq!(b.len(), 16 + 2 * 56);
        assert_eq!(u32::from_le_bytes(b[0..4].try_into().unwrap()), 2);
        let e1 = &b[16 + 56..];
        assert_eq!(u16::from_le_bytes([e1[0], e1[1]]), 1); // class copy
        assert_eq!(u64::from_le_bytes(e1[8..16].try_into().unwrap()), ENGINE_INFO_HAS_LOGICAL_INSTANCE);
        let m = memory_regions_blob(16 << 30, 1 << 30);
        assert_eq!(m.len(), 16 + 88);
        assert_eq!(u32::from_le_bytes(m[0..4].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(m[16 + 8..16 + 16].try_into().unwrap()), 16 << 30);
        assert_eq!(u64::from_le_bytes(m[16 + 16..16 + 24].try_into().unwrap()), 1 << 30);
        assert_eq!(u64::from_le_bytes(m[16 + 24..16 + 32].try_into().unwrap()), 0); // not small-BAR
    }

    fn eb() -> Execbuffer2 {
        Execbuffer2 { buffer_count: 3, flags: EXEC_NO_RELOC | EXEC_HANDLE_LUT | EXEC_BATCH_FIRST, ..Default::default() }
    }

    #[test]
    fn execbuf_checks() {
        let legacy = |r: u64| r == EXEC_DEFAULT || r == EXEC_RENDER || r == EXEC_BLT;
        let p = check_execbuf(&eb(), None, &legacy).unwrap();
        assert_eq!((p.engine, p.batch_index, p.fence_array), (0, 0, false));
        let mut e = eb();
        e.flags &= !EXEC_BATCH_FIRST;
        assert_eq!(check_execbuf(&e, None, &legacy).unwrap().batch_index, 2);
        e.flags |= EXEC_BSD;
        assert_eq!(check_execbuf(&e, None, &legacy), Err(EINVAL)); // no video engine
        let mut e = eb();
        e.flags |= 1; // engine map index 1
        assert_eq!(check_execbuf(&e, Some(2), &legacy).unwrap().engine, 1);
        assert_eq!(check_execbuf(&e, Some(1), &legacy), Err(EINVAL));
        let mut e = eb();
        e.batch_start_offset = 4;
        assert_eq!(check_execbuf(&e, None, &legacy), Err(EINVAL));
        let mut e = eb();
        e.flags |= 1 << 40;
        assert_eq!(check_execbuf(&e, None, &legacy), Err(EINVAL));
        let mut e = eb();
        e.num_cliprects = 2;
        assert_eq!(check_execbuf(&e, None, &legacy), Err(EINVAL)); // cliprects without FENCE_ARRAY
        e.flags |= EXEC_FENCE_ARRAY;
        assert!(check_execbuf(&e, None, &legacy).unwrap().fence_array);
        let mut e = eb();
        e.buffer_count = 0;
        assert_eq!(check_execbuf(&e, None, &legacy), Err(EINVAL));
    }

    #[test]
    fn exec_object_checks() {
        let vm = 1u64 << 48;
        let mut o = ExecObject2 { handle: 1, offset: 0x1_0000_0000, flags: EXEC_OBJECT_PINNED | EXEC_OBJECT_SUPPORTS_48B_ADDRESS, ..Default::default() };
        assert_eq!(check_exec_object(&o, vm, 4096), Ok(0x1_0000_0000));
        // Canonical form of an address with bit 47 set.
        o.offset = canonical(0xFFFF_0000_0000);
        assert_eq!(o.offset, 0xFFFF_FFFF_0000_0000);
        assert_eq!(check_exec_object(&o, vm, 4096), Ok(0xFFFF_0000_0000));
        o.offset = 0xFFFF_0000_0000; // not canonical
        assert_eq!(check_exec_object(&o, vm, 4096), Err(EINVAL));
        o.offset = 0x1000_0800;
        assert_eq!(check_exec_object(&o, vm, 4096), Err(EINVAL)); // misaligned
        o.offset = 0x1_0000_0000;
        o.flags = EXEC_OBJECT_PINNED; // no 48B flag above 4 GiB
        assert_eq!(check_exec_object(&o, vm, 4096), Err(EINVAL));
        o.flags = 0;
        o.offset = 0x1000;
        assert_eq!(check_exec_object(&o, vm, 4096), Err(EINVAL)); // relocation mode unsupported
        o.flags = EXEC_OBJECT_PINNED;
        o.relocation_count = 1;
        assert_eq!(check_exec_object(&o, vm, 4096), Err(EINVAL));
        o.relocation_count = 0;
        o.offset = canonical(vm - 4096) ;
        o.flags |= EXEC_OBJECT_SUPPORTS_48B_ADDRESS;
        assert_eq!(check_exec_object(&o, vm, 8192), Err(EINVAL)); // runs past the VM
    }

    #[test]
    fn context_params() {
        assert_eq!(context_getparam_static(CONTEXT_PARAM_GTT_SIZE), Some(1 << 48));
        assert_eq!(context_getparam_static(CONTEXT_PARAM_VM), None);
        let mut b = vec![0u8; 8];
        for (c, i) in [(0u16, 0u16), (1, 0)] { b.extend_from_slice(&c.to_le_bytes()); b.extend_from_slice(&i.to_le_bytes()); }
        assert_eq!(parse_engine_map(&b, &[(0, 0), (1, 0)]), Ok(vec![(0, 0), (1, 0)]));
        assert_eq!(parse_engine_map(&b, &[(0, 0)]), Err(EINVAL));
        b[0] = 1; // extensions
        assert_eq!(parse_engine_map(&b, &[(0, 0), (1, 0)]), Err(EINVAL));
        assert_eq!(parse_engine_map(&[0u8; 7], &[]), Err(EINVAL));
    }
}

#[cfg(test)]
mod g3b_tests {
    use super::hw::{self, cmd, EngineClass};
    use super::wa::*;

    fn find(l: &WaList, reg: u32) -> Wa { *l.0.iter().find(|w| w.reg == reg).expect("reg in list") }

    #[test]
    fn kbl_steppings() {
        assert_eq!(kbl_step(7), 6); // KBL-R G0 (the laptop)
        assert!(kbl_step(2) >= STEP_C0 && kbl_step(5) >= STEP_C0 && kbl_step(1) < STEP_C0);
        assert!(kbl_step(4) == STEP_F0 && kbl_step(6) < STEP_F0);
    }

    #[test]
    fn kbl_ctx_workarounds_match_linux() {
        let l = kbl_ctx_was(kbl_step(7));
        // Sorted by register, one LRI entry per register, all masked.
        let regs: Vec<u32> = l.0.iter().map(|w| w.reg).collect();
        assert_eq!(regs, [0x2580, 0x7004, 0x7014, 0x7300, 0xE100, 0xE180, 0xE184, 0xE188, 0xE194, 0xE4F0]);
        assert!(l.0.iter().all(|w| w.masked && w.clr == 0));
        let v = |r| find(&l, r).set;
        assert_eq!(v(0x2580), 0x0007_0004); // 3D object-level off; GPGPU command level
        assert_eq!(v(0x7004), 0x0042_0042);
        assert_eq!(v(0x7014), 0x2100_2100); // hash selection + push-const fix (>= C0)
        assert_eq!(v(0x7300), 0x8030_8030);
        assert_eq!(v(0xE100), 0x0010_0010);
        assert_eq!(v(0xE180), 0x2000_2000);
        assert_eq!(v(0xE184), 0x0002_0002);
        assert_eq!(v(0xE188), 0x0008_0000);
        assert_eq!(v(0xE194), 0x0114_0114);
        assert_eq!(v(0xE4F0), 0x8100_8100);
        // Before C0 the push-constant fix is not applied.
        assert_eq!(find(&kbl_ctx_was(1), 0x7014).set, 0x2000_2000);
        let lri = ctx_wa_lri(&l);
        assert_eq!(lri.len(), 1 + 2 * 10 + 1);
        assert_eq!(lri[0], 0x1100_0013); // MI_LOAD_REGISTER_IMM(10)
        assert_eq!(&lri[1..3], &[0x2580, 0x0007_0004]);
        assert_eq!(*lri.last().unwrap(), cmd::MI_NOOP);
    }

    #[test]
    fn kbl_gt_and_engine_workarounds_match_linux() {
        let g = kbl_gt_was(kbl_step(7), 0, 0);
        let regs: Vec<u32> = g.0.iter().map(|w| w.reg).collect();
        assert_eq!(regs, [0x0FDC, 0x4090, 0x4AB0, 0x4DDC, 0x940C]);
        let mcr = find(&g, 0x0FDC);
        assert_eq!((mcr.clr, mcr.set), (0x0F00_0000, 0));
        assert_eq!(mcr.value(0xFFFF_FFFF), 0xF0FF_FFFF);
        let eco = find(&g, 0x4090);
        assert_eq!((eco.clr, eco.set), (1 << 8 | 1 << 25, 1 << 8 | 1 << 25));
        assert_eq!(find(&g, 0x4DDC).set, 0x8800_0000);
        assert_eq!(find(&g, 0x4AB0).value(0x1), 0x0004_0001);
        // A0/B0 parts also disable dynamic credit sharing.
        assert_eq!(find(&kbl_gt_was(1, 0, 1), 0x4AB8).set, 1 << 28);
        assert_eq!(find(&kbl_gt_was(1, 0, 1), 0x0FDC).set, 1 << 24);

        let r = gen9_rcs_engine_was();
        let regs: Vec<u32> = r.0.iter().map(|w| w.reg).collect();
        assert_eq!(regs, [0x20D4, 0x20E0, 0xB004, 0xB008, 0xB118, 0xB11C]);
        assert_eq!(find(&r, 0x20D4).set, 0x0004_0004);
        assert_eq!(find(&r, 0x20E0).set, 0x4000_4000);
        let sq = find(&r, 0xB118);
        assert_eq!((sq.clr, sq.set), (1 << 21 | 1 << 22, 1 << 21));
        assert_eq!(sq.value(0xFFFF_FFFF), 0xFFBF_FFFF);
        let s1 = find(&r, 0xB11C);
        assert_eq!((s1.clr, s1.set), (1 << 2 | 1 << 8, 1 << 2));
        assert_eq!(find(&r, 0xB008).value(0x3), 0x2);
        assert!(s1.holds(0x4) && !s1.holds(0x104));
    }

    #[test]
    fn rcs_whitelist_slots() {
        let w = whitelist_writes(0x2000, &KBL_RCS_WHITELIST);
        assert_eq!(w.len(), 12);
        assert_eq!(w[0], (0x24D0, 0x2248));
        assert_eq!(w[4], (0x24E0, 0xB118));
        assert_eq!(w[5], (0x24E4, 0x2094));
        assert_eq!(w[11], (0x24FC, 0x2094));
    }

    #[test]
    fn skl_mocs_tables() {
        let m = mocs_writes(0xC800);
        assert_eq!(m.len(), 64);
        assert_eq!(m[0], (0xC800, 0x09));
        assert_eq!(m[1].1, 0x30);
        assert_eq!(m[2].1, 0x3B);
        assert_eq!(m[17].1, 0x30);
        assert_eq!(m[63], (0xC8FC, 0x37));
        let l = l3cc_writes();
        assert_eq!(l.len(), 32);
        assert_eq!(l[0], (0xB020, 0x0030_0010));
        assert_eq!(l[1], (0xB024, 0x0030_0030));
        assert_eq!(l[31], (0xB09C, 0x0010_0030));
    }

    #[test]
    fn null_render_state_relocation() {
        let base = 0xBF10_0000u64;
        let (d, len) = renderstate_batch(base).expect("relocs");
        assert_eq!(len, 960 * 4);
        assert_eq!(d.len() % 16, 0);
        assert_eq!(d[0], 0x7A00_0004); // PIPE_CONTROL
        assert_eq!(d[6], 0x6904_0300); // PIPELINE_SELECT 3D
        for &off in super::gen9_renderstate::GEN9_NULL_STATE_RELOCS.iter() {
            let i = off as usize / 4;
            let src = super::gen9_renderstate::GEN9_NULL_STATE_BATCH[i];
            assert_eq!(d[i] as u64 | (d[i + 1] as u64) << 32, src as u64 + base);
        }
        assert_eq!(d[959], super::gen9_renderstate::GEN9_NULL_STATE_BATCH[959]);
        assert!(d[960..].iter().all(|&x| x == cmd::MI_NOOP));
    }

    #[test]
    fn requests_and_ring_placement() {
        let bcs = hw::build_request(EngineClass::Copy, 0x1000_00D0, 0x1000_0100, 7, &[], Some((0x7_0000_1000, true)));
        assert_eq!(bcs.len() % 2, 0);
        assert_eq!(cmd::mi_opcode(bcs[0]), 0x26); // MI_FLUSH_DW invalidate first
        let bbs = bcs.iter().position(|&x| cmd::mi_opcode(x) == 0x31 && x >> 29 == 0).expect("bbs");
        assert_eq!(bcs[bbs] & cmd::MI_BATCH_PPGTT, cmd::MI_BATCH_PPGTT);
        assert_eq!((bcs[bbs + 1], bcs[bbs + 2]), (0x1000, 0x7));
        // Breadcrumb: MI_FLUSH_DW post-sync store of 7 to 0x1000_0100 after the batch.
        assert!(bcs[bbs..].windows(4).any(|w| cmd::mi_opcode(w[0]) == 0x26 && w[1] & !7 == 0x1000_0100 && w[3] == 7));
        let rcs = hw::build_request(EngineClass::Render, 0x10D0, 0x1100, 9, &[0xAAAA_0000], Some((0x2000, false)));
        let bbs = rcs.iter().position(|&x| cmd::mi_opcode(x) == 0x31 && x >> 29 == 0).expect("bbs");
        assert_eq!(rcs[bbs] & cmd::MI_BATCH_PPGTT, 0); // kernel batch from the GGTT
        let pre = rcs.iter().position(|&x| x == 0xAAAA_0000).expect("pre");
        assert!(pre < bbs);
        // Breadcrumb: the PIPE_CONTROL that writes 9 to 0x1100.
        assert!(rcs.windows(5).any(|w| w[0] == cmd::PIPE_CONTROL6 && w[2] == 0x1100 && w[4] == 9));
        assert_eq!(hw::ring_place(0, 256, 16384), Some((0, 0)));
        assert_eq!(hw::ring_place(16128, 256, 16384), Some((0, 16128)));
        assert_eq!(hw::ring_place(16200, 256, 16384), Some(((16384 - 16200) / 4, 0)));
        assert_eq!(hw::ring_place(4, 256, 16384), None);
        assert_eq!(hw::ring_place(0, 16384, 16384), None);
    }
}
