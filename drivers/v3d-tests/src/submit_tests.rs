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
