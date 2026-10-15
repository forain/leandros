//! G3b: what Linux programs into a Gen9 big-core (SKL/KBL) GT before user
//! work runs on it, as pure tables — so the host tests compare them with the
//! Linux values and the kernel side only replays them.
//!
//! * workaround lists (`intel_workarounds.c`, v6.12): the GT list (MMIO,
//!   applied once after reset), the render engine list (MMIO, re-applied
//!   after an engine reset), the render context list (LRIs in the golden
//!   context request, so every context inherits them) and the render
//!   engine's non-privileged whitelist (RING_FORCE_TO_NONPRIV);
//! * MOCS (`intel_mocs.c`): the SKL/KBL control table into each engine's
//!   MOCS registers, the L3CC table into LNCFCMOCS;
//! * the null render state batch (`intel_renderstate.c`), relocated.
//!
//! Only KBL is wired (the laptop is 8086:5917, KBL-R GT2, rev 7 = G0); the
//! SKL/CFL differences are the step-gated entries and noted where they are.

use alloc::vec::Vec;

use super::gen9_renderstate::{GEN9_NULL_STATE_BATCH, GEN9_NULL_STATE_RELOCS};
use super::hw::{cmd, masked_dis, masked_en};

/// One workaround: `reg = (reg & !clr) | set`, or for a masked register a
/// plain write of `set` (the upper half selects the bits). `read` is the
/// mask a verification read compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wa { pub reg: u32, pub clr: u32, pub set: u32, pub read: u32, pub masked: bool }

impl Wa {
    /// The value to write given the register's current value.
    pub fn value(&self, old: u32) -> u32 {
        if self.masked || self.clr == u32::MAX { self.set } else { (old & !self.clr) | self.set }
    }
    /// Does `cur` hold the workaround (`wa_verify`)?
    pub fn holds(&self, cur: u32) -> bool { (cur ^ self.set) & self.read == 0 }
    /// Does applying it need a read first?
    pub fn needs_read(&self) -> bool { !(self.masked || self.clr == u32::MAX) }
}

/// A workaround list with Linux's `_wa_add` semantics: sorted by register,
/// entries on the same register merged (set/clr/read OR-ed together).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WaList(pub Vec<Wa>);

impl WaList {
    pub fn add(&mut self, w: Wa) {
        match self.0.binary_search_by_key(&w.reg, |x| x.reg) {
            Ok(i) => {
                let x = &mut self.0[i];
                // Linux drops the overwritten set bits when the new entry
                // clears everything the old one did.
                if (w.clr | x.clr) != 0 && w.clr & !x.clr == 0 { x.set &= !w.clr; }
                x.set |= w.set;
                x.clr |= w.clr;
                x.read |= w.read;
            }
            Err(i) => self.0.insert(i, w),
        }
    }
    fn clr_set(&mut self, reg: u32, clr: u32, set: u32) { self.add(Wa { reg, clr, set, read: clr | set, masked: false }); }
    fn or(&mut self, reg: u32, set: u32) { self.clr_set(reg, set, set); }
    fn clr(&mut self, reg: u32, clr: u32) { self.clr_set(reg, clr, 0); }
    fn m_en(&mut self, reg: u32, v: u32) { self.add(Wa { reg, clr: 0, set: masked_en(v), read: v, masked: true }); }
    fn m_dis(&mut self, reg: u32, v: u32) { self.add(Wa { reg, clr: 0, set: masked_dis(v), read: v, masked: true }); }
    fn m_field(&mut self, reg: u32, mask: u32, v: u32) { self.add(Wa { reg, clr: 0, set: mask << 16 | v, read: mask, masked: true }); }
}

/// Registers the lists touch (gt/intel_gt_regs.h).
pub mod r {
    pub const GEN8_MCR_SELECTOR: u32 = 0x0FDC;
    pub const GEN7_FF_SLICE_CS_CHICKEN1: u32 = 0x20E0;
    pub const GEN9_CSFE_CHICKEN1_RCS: u32 = 0x20D4;
    pub const GEN9_CTX_PREEMPT_REG: u32 = 0x2248;
    pub const GEN8_CS_CHICKEN1: u32 = 0x2580;
    pub const GAM_ECOCHK: u32 = 0x4090;
    pub const GAMT_CHKN_BIT_REG: u32 = 0x4AB8;
    pub const GEN9_GAMT_ECO_REG_RW_IA: u32 = 0x4AB0;
    pub const GEN8_L3_LRA_1_GPGPU: u32 = 0x4DD4;
    pub const MMCD_MISC_CTRL: u32 = 0x4DDC;
    pub const CACHE_MODE_1: u32 = 0x7004;
    pub const COMMON_SLICE_CHICKEN2: u32 = 0x7014;
    pub const HDC_CHICKEN0: u32 = 0x7300;
    pub const GEN8_HDC_CHICKEN1: u32 = 0x7304;
    pub const GEN7_UCGCTL4: u32 = 0x940C;
    pub const GEN8_GARBCNTL: u32 = 0xB004;
    pub const GEN9_SCRATCH_LNCF1: u32 = 0xB008;
    pub const GEN9_LNCFCMOCS0: u32 = 0xB020;
    pub const GEN8_L3SQCREG4: u32 = 0xB118;
    /// BDW_SCRATCH1 and GEN9_SCRATCH1 (one register).
    pub const GEN9_SCRATCH1: u32 = 0xB11C;
    pub const GEN9_RCS0_MOCS0: u32 = 0xC800;
    pub const GEN9_BCS0_MOCS0: u32 = 0xCC00;
    pub const GEN8_HALF_SLICE_CHICKEN1: u32 = 0xE100;
    pub const HALF_SLICE_CHICKEN2: u32 = 0xE180;
    pub const GEN8_HALF_SLICE_CHICKEN3: u32 = 0xE184;
    pub const GEN9_HALF_SLICE_CHICKEN5: u32 = 0xE188;
    pub const GEN9_HALF_SLICE_CHICKEN7: u32 = 0xE194;
    pub const GEN8_ROW_CHICKEN: u32 = 0xE4F0;
    /// Engine-relative.
    pub const RING_NOPID: u32 = 0x94;
    pub const RING_FORCE_TO_NONPRIV: u32 = 0x4D0;
    pub const RING_MAX_NONPRIV_SLOTS: u32 = 12;
}

/// KBL graphics stepping from the PCI revision (`kbl_revids`): the index
/// into A0, B0, C0, D0, F0, C0, D1, G0 as an ordinal (A0 = 0 ... G0 = 7);
/// unknown revisions count as the newest.
pub fn kbl_step(rev: u8) -> u8 {
    // A0 B0 C0 D0 D1 F0 G0 -> 0 1 2 3 4 5 6 (ordinal, D1 between D0 and F0)
    match rev { 0 => 0, 1 => 1, 2 | 5 => 2, 3 => 3, 6 => 4, 4 => 5, _ => 6 }
}
pub const STEP_C0: u8 = 2;
pub const STEP_F0: u8 = 5;

/// `kbl_ctx_workarounds_init` (render context; HAS_LLC).
pub fn kbl_ctx_was(step: u8) -> WaList {
    let mut w = WaList::default();
    // gen9_ctx_workarounds_init
    w.m_en(r::COMMON_SLICE_CHICKEN2, 1 << 13); // GEN9_PBE_COMPRESSED_HASH_SELECTION
    w.m_en(r::GEN9_HALF_SLICE_CHICKEN7, 1 << 8); // GEN9_SAMPLER_HASH_COMPRESSED_READ_ADDR
    w.m_en(r::GEN8_ROW_CHICKEN, 1 << 15 | 1 << 8); // FLOW_CONTROL_ENABLE | PARTIAL_INSTRUCTION_SHOOTDOWN_DISABLE
    w.m_en(r::GEN9_HALF_SLICE_CHICKEN7, 1 << 4 | 1 << 2); // YV12_BUGFIX | GPGPU_PREEMPTION
    w.m_en(r::CACHE_MODE_1, 1 << 6 | 1 << 1); // 4x4_STC_OPTIMIZATION_DISABLE | PARTIAL_RESOLVE_IN_VC_DISABLE
    w.m_dis(r::GEN9_HALF_SLICE_CHICKEN5, 1 << 3); // CCS_TLB_PREFETCH_ENABLE
    w.m_en(r::HDC_CHICKEN0, 1 << 5 | 1 << 15); // FORCE_CONTEXT_SAVE_RESTORE_NON_COHERENT | CSR_NON_COHERENT_OVR_DISABLE
    w.m_en(r::HDC_CHICKEN0, 1 << 4); // HDC_FORCE_NON_COHERENT
    w.m_en(r::GEN8_HALF_SLICE_CHICKEN3, 1 << 1); // GEN8_SAMPLER_POWER_BYPASS_DIS (skl/kbl/cfl/cml)
    w.m_en(r::HALF_SLICE_CHICKEN2, 1 << 13); // GEN8_ST_PO_DISABLE
    w.m_dis(r::GEN8_CS_CHICKEN1, 1 << 0); // GEN9_PREEMPT_3D_OBJECT_LEVEL
    w.m_field(r::GEN8_CS_CHICKEN1, 0x6, 0x4); // GPGPU preemption level mask / command level
    // kbl
    if step >= STEP_C0 { w.m_en(r::COMMON_SLICE_CHICKEN2, 1 << 8); } // SBE_DISABLE_REPLAY_BUF_OPTIMIZATION
    w.m_en(r::GEN8_HALF_SLICE_CHICKEN1, 1 << 4); // SBE_SS_CACHE_DISPATCH_PORT_SHARING_DISABLE
    w
}

/// `kbl_gt_workarounds_init`. `(slice, subslice)` = the first enabled pair
/// (WaProgramMgsrForCorrectSliceSpecificMmioReads).
pub fn kbl_gt_was(step: u8, slice: u32, subslice: u32) -> WaList {
    let mut w = WaList::default();
    w.clr_set(r::GEN8_MCR_SELECTOR, 3 << 26 | 3 << 24, (slice & 3) << 26 | (subslice & 3) << 24);
    w.or(r::GAM_ECOCHK, 1 << 8); // ECOCHK_DIS_TLB (not cfl/cml)
    w.or(r::MMCD_MISC_CTRL, 1 << 31 | 1 << 27); // MMCD_PCLA | MMCD_HOTSPOT_EN (HAS_LLC)
    w.or(r::GAM_ECOCHK, 1 << 25); // BDW_DISABLE_HDC_INVALIDATION
    if step < STEP_C0 { w.or(r::GAMT_CHKN_BIT_REG, 1 << 28); } // DISABLE_DYNAMIC_CREDIT_SHARING
    w.or(r::GEN7_UCGCTL4, 1 << 14); // GEN8_EU_GAUNIT_CLOCK_GATE_DISABLE
    w.or(r::GEN9_GAMT_ECO_REG_RW_IA, 1 << 18); // GAMT_ECO_ENABLE_IN_PLACE_DECOMPRESS
    w
}

/// `gtt_write_workarounds` for gen9 big core: (register, value), plain write.
pub const GTT_WAS: [(u32, u32); 1] = [(r::GEN8_L3_LRA_1_GPGPU, 0x67F1_427F)];

/// `rcs_engine_wa_init`, the entries that apply to gen9 big core.
pub fn gen9_rcs_engine_was() -> WaList {
    let mut w = WaList::default();
    w.m_en(r::GEN7_FF_SLICE_CS_CHICKEN1, 1 << 14); // GEN9_FFSC_PERCTX_PREEMPT_CTRL
    w.or(r::GEN8_GARBCNTL, 1 << 7); // GEN9_GAPS_TSV_CREDIT_DISABLE (skl/kbl/cfl/cml)
    w.m_en(r::GEN9_CSFE_CHICKEN1_RCS, 1 << 2); // PREEMPT_GPGPU_SYNC_SWITCH_DISABLE
    w.or(r::GEN9_SCRATCH1, 1 << 2); // LBS_SLA_RETRY_TIMER_DECREMENT_ENABLE
    w.or(r::GEN8_L3SQCREG4, 1 << 21); // LQSC_FLUSH_COHERENT_LINES
    w.clr(r::GEN9_SCRATCH_LNCF1, 1 << 0); // NONIA_COHERENT_ATOMICS_ENABLE
    w.clr(r::GEN8_L3SQCREG4, 1 << 22); // LQSQ_NONIA_COHERENT_ATOMICS_ENABLE
    w.clr(r::GEN9_SCRATCH1, 1 << 8); // EVICTION_PERF_FIX_ENABLE
    w
}

/// `kbl_whitelist_build` for the render engine, in slot order (the list is
/// address-sorted like every wa list). Copy engines get none.
pub const KBL_RCS_WHITELIST: [u32; 5] = [
    r::GEN9_CTX_PREEMPT_REG, r::GEN8_CS_CHICKEN1, r::COMMON_SLICE_CHICKEN2, r::GEN8_HDC_CHICKEN1, r::GEN8_L3SQCREG4,
];

/// The RING_FORCE_TO_NONPRIV writes for an engine at `base`: (register,
/// value) for all 12 slots, unused ones pointing at the harmless RING_NOPID.
pub fn whitelist_writes(base: u32, list: &[u32]) -> Vec<(u32, u32)> {
    (0..r::RING_MAX_NONPRIV_SLOTS)
        .map(|i| (base + r::RING_FORCE_TO_NONPRIV + 4 * i, list.get(i as usize).copied().unwrap_or(base + r::RING_NOPID)))
        .collect()
}

/// The context workaround request body (`intel_engine_emit_ctx_wa`, minus
/// the barrier flush the caller emits first): one LRI of every entry
/// (all masked here, so no reads), padded with a NOOP.
pub fn ctx_wa_lri(list: &WaList) -> Vec<u32> {
    let mut cs = Vec::new();
    if list.0.is_empty() { return cs; }
    cs.push(cmd::lri_header(list.0.len() as u32));
    for w in &list.0 {
        cs.push(w.reg);
        cs.push(w.set);
    }
    cs.push(cmd::MI_NOOP);
    cs
}

/// `gen9_init_indirectctx_bb` (intel_lrc.c, v6.12): the render engine's
/// indirect context workaround batch, run by the hardware on every restore
/// of a context whose image points at it. `scratch` is a GGTT address of
/// two free qwords (the GEN8_L3SQCREG4 save and the SLM-clear post-sync
/// write; Linux uses the GT scratch page and the PPHWSP). Padded to a whole
/// number of 64-byte cachelines (the image encodes the size in cachelines).
pub fn gen9_indirect_ctx_bb(scratch: u32) -> Vec<u32> {
    const MI_SRM_GEN8: u32 = cmd::mi(0x24, 2);
    const MI_LRM_GEN8: u32 = cmd::mi(0x29, 2);
    const MI_SRM_LRM_GLOBAL_GTT: u32 = 1 << 22;
    const PC_FLUSH_L3: u32 = 1 << 27;
    const FF_SLICE_CHICKEN: u32 = 0x2088;
    const _3D_CHICKEN3: u32 = 0x2090;
    let mut b = Vec::new();
    b.push(cmd::MI_ARB_ON_OFF | cmd::MI_ARB_DISABLE);
    // WaFlushCoherentL3CacheLinesAtContextSwitch (gen8_emit_flush_coherentl3_wa)
    b.extend_from_slice(&[MI_SRM_GEN8 | MI_SRM_LRM_GLOBAL_GTT, r::GEN8_L3SQCREG4, scratch, 0]);
    b.extend_from_slice(&[cmd::lri_header(1), r::GEN8_L3SQCREG4, 0x4040_0000 | 1 << 21]);
    cmd::pipe_control(&mut b, cmd::PC_CS_STALL | cmd::PC_DC_FLUSH_ENABLE, 0, 0);
    b.extend_from_slice(&[MI_LRM_GEN8 | MI_SRM_LRM_GLOBAL_GTT, r::GEN8_L3SQCREG4, scratch, 0]);
    // WaClearSlmSpaceAtContextSwitch
    cmd::pipe_control(&mut b, PC_FLUSH_L3 | cmd::PC_GLOBAL_GTT_IVB | cmd::PC_CS_STALL | cmd::PC_QW_WRITE, scratch + 8, 0);
    // WaDisableGatherAtSetShaderCommonSlice; CL and SF provoking-vertex fixes (BSpec 11391, 11299)
    b.extend_from_slice(&[cmd::lri_header(3),
        r::COMMON_SLICE_CHICKEN2, (1 << 12) << 16,
        FF_SLICE_CHICKEN, masked_en(1 << 1),
        _3D_CHICKEN3, masked_en(1 << 12)]);
    b.push(cmd::MI_ARB_ON_OFF | cmd::MI_ARB_ENABLE);
    while b.len() % 16 != 0 { b.push(cmd::MI_NOOP); }
    b
}

// ── MOCS ─────────────────────────────────────────────────────────────────────

pub const GEN9_NUM_MOCS_ENTRIES: u32 = 64;

/// `skl_mocs_table` (also KBL/CFL): entry -> (control, l3cc); unused
/// entries take I915_MOCS_PTE's values.
pub fn skl_mocs(i: u32) -> (u32, u16) {
    const LE_LRUM3: u32 = 3 << 4;
    match i {
        0 => (1 | 2 << 2, 1 << 4),             // UNCACHED: LE_1_UC | LE_TC_2_LLC_ELLC, L3_1_UC
        2 => (3 | 2 << 2 | LE_LRUM3, 3 << 4),  // CACHED: LE_3_WB | LE_TC_2_LLC_ELLC | LRUM(3), L3_3_WB
        63 => (3 | 1 << 2 | LE_LRUM3, 1 << 4), // LE_3_WB | LE_TC_1_LLC | LRUM(3), L3_1_UC
        _ => (LE_LRUM3, 3 << 4),               // PTE: LE_0_PAGETABLE | LE_TC_0_PAGETABLE | LRUM(3), L3_3_WB
    }
}

/// The MOCS control writes for one engine (`init_mocs_table`).
pub fn mocs_writes(mocs0: u32) -> Vec<(u32, u32)> {
    (0..GEN9_NUM_MOCS_ENTRIES).map(|i| (mocs0 + 4 * i, skl_mocs(i).0)).collect()
}

/// The LNCFCMOCS writes (`init_l3cc_table`): two 16-bit entries per register.
pub fn l3cc_writes() -> Vec<(u32, u32)> {
    (0..GEN9_NUM_MOCS_ENTRIES / 2)
        .map(|i| (r::GEN9_LNCFCMOCS0 + 4 * i, skl_mocs(2 * i).1 as u32 | (skl_mocs(2 * i + 1).1 as u32) << 16))
        .collect()
}

// ── null render state ────────────────────────────────────────────────────────

/// `render_state_setup`: the gen9 null-state batch relocated to the GGTT
/// address `base` (64-bit relocations), padded to a cacheline. Returns the
/// dwords and the batch length in bytes that is dispatched (the aux batch
/// is a lone MI_BATCH_BUFFER_END on non-pooled-EU parts and is not run).
pub fn renderstate_batch(base: u64) -> Option<(Vec<u32>, u32)> {
    let src = &GEN9_NULL_STATE_BATCH;
    let mut d = Vec::with_capacity(src.len() + 16);
    let mut ri = 0;
    let mut i = 0;
    while i < src.len() {
        let s = src[i];
        if ri < GEN9_NULL_STATE_RELOCS.len() && (i * 4) as u32 == GEN9_NULL_STATE_RELOCS[ri] {
            if i + 1 >= src.len() || src[i + 1] != 0 { return None; }
            let a = s as u64 + base;
            d.push(a as u32);
            d.push((a >> 32) as u32);
            i += 2;
            ri += 1;
            continue;
        }
        d.push(s);
        i += 1;
    }
    if ri != GEN9_NULL_STATE_RELOCS.len() { return None; }
    let len = (src.len() * 4) as u32;
    while d.len() % 16 != 0 { d.push(cmd::MI_NOOP); }
    Some((d, len))
}
