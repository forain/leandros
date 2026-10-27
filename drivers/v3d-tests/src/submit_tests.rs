//! SUBMIT_CL argument parsing, the MULTI_SYNC extension chain, and the self-test CL image.
use crate::v3d::selftest_cl::*;
use crate::v3d::submit::*;
use std::collections::HashMap;

fn args(f: impl FnOnce(&mut [u8; 72])) -> ClArgs {
    let mut b = [0u8; 72];
    b[8..12].copy_from_slice(&0x2000u32.to_le_bytes());
    b[12..16].copy_from_slice(&0x2100u32.to_le_bytes());
    f(&mut b);
    ClArgs::parse(&b)
}
fn put32(b: &mut [u8], o: usize, v: u32) { b[o..o + 4].copy_from_slice(&v.to_le_bytes()); }
fn put64(b: &mut [u8], o: usize, v: u64) { b[o..o + 8].copy_from_slice(&v.to_le_bytes()); }

/// A fake user address space.
struct Mem(HashMap<u64, Vec<u8>>);
impl Mem {
    fn read(&self, a: u64, buf: &mut [u8]) -> bool {
        for (&base, v) in &self.0 {
            if a >= base && a + buf.len() as u64 <= base + v.len() as u64 {
                let o = (a - base) as usize;
                buf.copy_from_slice(&v[o..o + buf.len()]);
                return true;
            }
        }
        false
    }
}
fn sems(handles: &[u32]) -> Vec<u8> {
    let mut v = vec![0u8; 32 * handles.len()];
    for (i, &h) in handles.iter().enumerate() { put32(&mut v, i * 32, h); }
    v
}
fn ms(next: u64, ins: u64, outs: u64, n_in: u32, n_out: u32, stage: u32) -> Vec<u8> {
    let mut v = vec![0u8; 48];
    put64(&mut v, 0, next); put32(&mut v, 8, EXT_ID_MULTI_SYNC);
    put64(&mut v, 16, ins); put64(&mut v, 24, outs);
    put32(&mut v, 32, n_in); put32(&mut v, 36, n_out); put32(&mut v, 40, stage);
    v
}

#[test]
fn parse_offsets_match_uapi() {
    let a = args(|b| {
        for i in 0..10 { put32(b, i * 4, 0x100 + i as u32); }
        put64(b, 40, 0xdead_beef_0000); put32(b, 48, 3); put32(b, 52, 1); put32(b, 56, 0); put64(b, 64, 0);
    });
    assert_eq!((a.bcl_start, a.rcl_end, a.in_sync_bcl, a.in_sync_rcl, a.out_sync, a.qts), (0x100, 0x103, 0x104, 0x105, 0x106, 0x109));
    assert_eq!((a.bo_handles, a.bo_handle_count, a.flags), (0xdead_beef_0000, 3, SUBMIT_CL_FLUSH_CACHE));
}

#[test]
fn validate_rejects_bad_args() {
    assert!(args(|_| {}).validate().is_ok());
    assert_eq!(args(|b| put32(b, 60, 1)).validate(), Err(SubmitError::Inval));
    assert_eq!(args(|b| put32(b, 52, 4)).validate(), Err(SubmitError::Inval));
    assert_eq!(args(|b| put32(b, 52, SUBMIT_EXTENSION)).validate(), Err(SubmitError::Inval), "flag without chain");
    assert_eq!(args(|b| put64(b, 64, 0x1000)).validate(), Err(SubmitError::Inval), "chain without flag");
    assert_eq!(args(|b| { put32(b, 0, 0x10); put32(b, 4, 0x8); }).validate(), Err(SubmitError::Inval));
    assert_eq!(args(|b| put32(b, 12, 0x2000)).validate(), Err(SubmitError::Inval), "empty RCL");
}

#[test]
fn legacy_syncs_without_extension() {
    let a = args(|b| { put32(b, 16, 5); put32(b, 20, 6); put32(b, 24, 7); });
    let p = plan_syncs(&a, &mut |_, _| panic!("no user reads")).unwrap();
    assert_eq!((p.bin_in, p.render_in, p.out, p.multisync), (vec![5], vec![6], vec![7], false));
}

#[test]
fn multisync_routes_by_wait_stage_and_ignores_legacy() {
    for (stage, bin, render) in [(QUEUE_BIN, vec![11, 12], vec![]), (QUEUE_RENDER, vec![], vec![11, 12])] {
        let mem = Mem(HashMap::from([(0x1000, ms(0, 0x2000, 0x3000, 2, 1, stage)), (0x2000, sems(&[11, 12])), (0x3000, sems(&[21]))]));
        let a = args(|b| { put32(b, 16, 5); put32(b, 20, 6); put32(b, 24, 7); put32(b, 52, SUBMIT_EXTENSION); put64(b, 64, 0x1000); });
        let p = plan_syncs(&a, &mut |x, buf| mem.read(x, buf)).unwrap();
        assert_eq!((p.bin_in, p.render_in, p.out, p.multisync), (bin, render, vec![21], true));
    }
}

#[test]
fn multisync_rejects_timelines_unknown_ids_dupes_and_faults() {
    let base = |stage| HashMap::from([(0x1000u64, ms(0, 0x2000, 0, 1, 0, stage)), (0x2000, sems(&[11]))]);
    let a = args(|b| { put32(b, 52, SUBMIT_EXTENSION); put64(b, 64, 0x1000); });
    let mut m = base(0); put64(m.get_mut(&0x2000).unwrap(), 8, 1); // point != 0
    assert_eq!(plan_syncs(&a, &mut |x, buf| Mem(m.clone()).read(x, buf)), Err(SubmitError::Inval));
    let mut m = base(0); put32(m.get_mut(&0x1000).unwrap(), 8, 2); // unknown id
    assert_eq!(plan_syncs(&a, &mut |x, buf| Mem(m.clone()).read(x, buf)), Err(SubmitError::Inval));
    let mut m = base(0); put64(m.get_mut(&0x1000).unwrap(), 0, 0x1000); // chained to itself: dup
    assert_eq!(plan_syncs(&a, &mut |x, buf| Mem(m.clone()).read(x, buf)), Err(SubmitError::Inval));
    let m = base(9); // wait_stage out of range
    assert_eq!(plan_syncs(&a, &mut |x, buf| Mem(m.clone()).read(x, buf)), Err(SubmitError::Inval));
    let m = HashMap::from([(0x1000u64, ms(0, 0x9000, 0, 1, 0, 0))]);
    assert_eq!(plan_syncs(&a, &mut |x, buf| Mem(m.clone()).read(x, buf)), Err(SubmitError::Fault));
}

#[test]
fn crc32_reference_vector() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    let img: Vec<u8> = (0..W * H).flat_map(|_| CLEAR_COLOR.to_le_bytes()).collect();
    assert_eq!(crc32(&img), selftest_expected_crc());
}

#[test]
fn selftest_cl_layout_and_patching() {
    let va = 0x0040_0000u32;
    let mut bo = vec![0u8; BO_SIZE];
    let (rs, re) = selftest_fill(&mut bo, va);
    assert_eq!((rs, re), (va, va + RCL.len() as u32));
    let rd = |o: usize| u32::from_le_bytes([bo[o], bo[o + 1], bo[o + 2], bo[o + 3]]);
    // First packet: TILE_RENDERING_MODE_CFG (Common), opcode 121, sub_id 0, 1 RT, 64x64, 32x32 tiles.
    assert_eq!(bo[0], 121);
    assert_eq!(bo[1] & 0x7, 0);
    assert_eq!(u16::from_le_bytes([bo[2], bo[3]]), 64);
    assert_eq!(u16::from_le_bytes([bo[4], bo[5]]), 64);
    // Every patched word now points into the BO at its intended offset.
    let targets: Vec<u32> = PATCHES.iter().map(|&p| rd(p) & !0x3f).collect();
    assert!(targets.contains(&(va + TLB_OFF as u32)));
    assert!(targets.contains(&(va + GTL_OFF as u32)));
    assert!(PATCHES.iter().any(|&p| rd(p) == va + GTL_OFF as u32 + GTL.len() as u32), "tile list end (unaligned, full word)");
    assert!(targets.contains(&(va + IMG_OFF as u32)));
    // Generic tile list ends with END_OF_TILE_MARKER (27), RETURN_FROM_SUB_LIST (18); RCL with END_OF_RENDERING (13).
    assert_eq!(&GTL[GTL.len() - 2..], &[27, 18]);
    assert_eq!(RCL[RCL.len() - 1], 13);
    assert!(IMG_OFF + (STRIDE * H) as usize <= BO_SIZE);
}

// ── phase D: SUBMIT_TFU / SUBMIT_CSD ──

fn le(buf: &mut [u8], o: usize, v: u32) { buf[o..o + 4].copy_from_slice(&v.to_le_bytes()); }

#[test]
fn tfu_args_layout() {
    let mut b = [0u8; SUBMIT_TFU_SIZE];
    for i in 0..7 { le(&mut b, 4 * i, 0x100 + i as u32); }
    for i in 0..4 { le(&mut b, 28 + 4 * i, 0x200 + i as u32); }
    le(&mut b, 44, 11); le(&mut b, 48, 12); le(&mut b, 52, 0); le(&mut b, 56, 14);
    le(&mut b, 60, 21); le(&mut b, 64, 22); le(&mut b, 68, SUBMIT_EXTENSION); le(&mut b, 72, 0xdead0);
    le(&mut b, 80, 0x8_6000);
    let a = TfuArgs::parse(&b);
    let r = a.regs;
    assert_eq!((r.icfg, r.iia, r.iis, r.ica, r.iua, r.ioa, r.ios), (0x100, 0x101, 0x102, 0x103, 0x104, 0x105, 0x106));
    assert_eq!(r.coef, [0x200, 0x201, 0x202, 0x203]);
    assert_eq!(r.ioc, 0x8_6000, "v71.ioc at 80");
    assert_eq!(a.handles(), vec![11, 12], "list ends at the first 0");
    assert_eq!((a.in_sync, a.out_sync, a.extensions), (21, 22, 0xdead0));
    assert!(a.validate().is_ok());
    le(&mut b, 68, 1); // FLUSH_CACHE is not a TFU flag
    assert_eq!(TfuArgs::parse(&b).validate(), Err(SubmitError::Inval));
    le(&mut b, 68, 0); // extension pointer without the flag
    assert_eq!(TfuArgs::parse(&b).validate(), Err(SubmitError::Inval));
}

#[test]
fn csd_args_layout() {
    let mut b = [0u8; SUBMIT_CSD_SIZE];
    for i in 0..7 { le(&mut b, 4 * i, 0x300 + i as u32); }
    le(&mut b, 44, 0xffff_ffff); // padding after coef[4]: ignored
    le(&mut b, 48, 0x1234_5678); le(&mut b, 52, 0x9);
    le(&mut b, 56, 3); le(&mut b, 60, 5); le(&mut b, 64, 6); le(&mut b, 68, 0); le(&mut b, 80, 0);
    let a = CsdArgs::parse(&b);
    assert_eq!(a.cfg, [0x300, 0x301, 0x302, 0x303, 0x304, 0x305, 0x306]);
    assert_eq!((a.bo_handles, a.bo_handle_count, a.in_sync, a.out_sync), (0x9_1234_5678, 3, 5, 6));
    assert!(a.validate().is_ok());
    le(&mut b, 80, 4);
    assert_eq!(CsdArgs::parse(&b).validate(), Err(SubmitError::Inval));
}

#[test]
fn single_queue_sync_plans() {
    let mut nomem = |_p: u64, _b: &mut [u8]| -> bool { false };
    let p = plan_syncs_single(0, 0, 3, 4, &mut nomem).unwrap();
    assert_eq!((p.bin_in.clone(), p.out.clone(), p.multisync), (vec![3], vec![4], false));
    // MULTI_SYNC with wait_stage = CACHE_CLEAN still gates the whole job; legacy fields ignored.
    let mut mem = std::collections::HashMap::<u64, u8>::new();
    let mut put = |a: u64, bytes: &[u8]| for (i, &x) in bytes.iter().enumerate() { mem.insert(a + i as u64, x); };
    let mut ext = [0u8; MULTI_SYNC_SIZE];
    le(&mut ext, 8, EXT_ID_MULTI_SYNC);
    ext[16..24].copy_from_slice(&0x2000u64.to_le_bytes());
    ext[24..32].copy_from_slice(&0x3000u64.to_le_bytes());
    le(&mut ext, 32, 1); le(&mut ext, 36, 1); le(&mut ext, 40, QUEUE_CACHE_CLEAN);
    put(0x1000, &ext);
    let mut s = [0u8; SEM_SIZE]; le(&mut s, 0, 8); put(0x2000, &s);
    le(&mut s, 0, 9); put(0x3000, &s);
    let mut rd = |p: u64, b: &mut [u8]| -> bool {
        for (i, x) in b.iter_mut().enumerate() { match mem.get(&(p + i as u64)) { Some(&v) => *x = v, None => return false } }
        true
    };
    let p = plan_syncs_single(SUBMIT_EXTENSION, 0x1000, 3, 4, &mut rd).unwrap();
    assert_eq!((p.bin_in, p.out, p.multisync), (vec![8], vec![9], true));
    assert_eq!(plan_syncs_single(SUBMIT_EXTENSION, 0x7000, 0, 0, &mut rd), Err(SubmitError::Fault));
}

#[test]
fn tfu_selftest_block_and_check() {
    let r = tfu_selftest_regs(0x40_0000);
    assert_eq!(r.icfg, 29 << 16, "raster in, R32F");
    assert_eq!((r.iia, r.ioa, r.iis, r.ios), (0x40_0000, 0x40_4000, 64, 0x0040_0040));
    assert_eq!(r.ioc, (6 << 12) | (8 << 16), "UIF_NO_XOR, stride 8 UIF blocks");
    let mut bo = vec![0u8; TFU_BO_SIZE];
    tfu_selftest_fill(&mut bo);
    // A plain copy and a reversed copy are both full permutations.
    let src = bo[..0x4000].to_vec();
    assert_eq!(tfu_selftest_check(&src), (4096, 0, 0));
    let mut rev: Vec<u8> = src.chunks_exact(4).rev().flatten().copied().collect();
    assert_eq!(tfu_selftest_check(&rev), (4096, 0, 0));
    let w1 = rev[4..8].to_vec();
    rev[0..4].copy_from_slice(&w1);
    assert_eq!(tfu_selftest_check(&rev), (4095, 1, 0));
    assert_eq!(tfu_selftest_check(&bo[0x4000..]), (0, 0, 4096), "untouched destination");
}

#[test]
fn auto_arm_only_with_mmu_and_not_opted_out() {
    assert!(auto_arm_decision(true, ""));
    assert!(auto_arm_decision(true, "console=ttyAMA10 v3d.selftest=0"));
    assert!(!auto_arm_decision(false, ""));
    assert!(!auto_arm_decision(true, "quiet v3d.arm=0"));
    assert!(auto_arm_decision(true, "v3d.arm=1"));
}
