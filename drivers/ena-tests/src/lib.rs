//! Host tests for the kernel's ENA driver core (`drivers/src/ena_hw.rs`),
//! included verbatim, so these exercise exactly the code the kernel runs. The
//! device is a Rust mock: a register file, an admin SQ/CQ processor that
//! executes the commands the driver sends, an AENQ generator, TX/RX rings over
//! simulated DMA memory (a "physical address" is a host pointer), a readless
//! MMIO mode that rejects plain register reads like an EC2 VF, and a TX ->
//! RX loopback. The mock encodes the ENA specification as the driver
//! understands it; real EC2 is the only judge of that (gate G1).

extern crate alloc;

#[path = "../../src/ena_hw.rs"]
pub mod ena_hw;

#[cfg(test)]
mod mock {
    use super::ena_hw::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    pub const TX_SQ_DB: usize = 0x1000;
    pub const RX_SQ_DB: usize = 0x1004;
    pub const TX_CQ_DB: usize = 0x1100;
    pub const RX_CQ_DB: usize = 0x1104;
    pub const TX_CQ_UNMASK: usize = 0x1200;
    pub const RX_CQ_UNMASK: usize = 0x1204;
    pub const MAC: [u8; 6] = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];

    unsafe fn p8(pa: u64, o: usize) -> *mut u8 { (pa as *mut u8).add(o) }
    unsafe fn r16(pa: u64, o: usize) -> u16 { std::ptr::read_volatile(p8(pa, o) as *const u16) }
    unsafe fn r32(pa: u64, o: usize) -> u32 { std::ptr::read_volatile(p8(pa, o) as *const u32) }
    unsafe fn w8(pa: u64, o: usize, v: u8) { std::ptr::write_volatile(p8(pa, o), v) }
    unsafe fn w16(pa: u64, o: usize, v: u16) { std::ptr::write_volatile(p8(pa, o) as *mut u16, v) }
    unsafe fn w32(pa: u64, o: usize, v: u32) { std::ptr::write_volatile(p8(pa, o) as *mut u32, v) }

    #[derive(Default)]
    pub struct Q {
        pub sq_ba: u64,
        pub cq_ba: u64,
        pub depth: u16,
        pub cq_words: u8,
        /// Device-side consumer of the SQ (free-running) and its phase.
        pub proc: u16,
        pub sq_phase: bool,
        /// Last tail the driver wrote to the SQ doorbell.
        pub posted: u16,
        pub cq_tail: u16,
        pub cq_phase: bool,
        pub created: bool,
        pub cq_head_db: u32,
        /// MSI-X mode: the CQ was created with interrupt_mode_enabled and this table entry.
        pub irq_mode: bool,
        pub msix_vector: u32,
        /// The CQ's interrupt is masked (the device masks after firing; the driver unmasks).
        pub irq_masked: bool,
        /// CQ entries the interrupt logic has already signalled.
        pub cq_seen: u16,
        /// Interrupts fired for this CQ.
        pub irqs: u32,
        /// LLQ: entry size in bytes when the SQ was created with DEV placement.
        pub llq_entry: usize,
    }

    impl Q {
        /// A completion became visible: raise the CQ's interrupt unless it is masked,
        /// then mask it (the device auto-masks after firing).
        pub fn fire(&mut self) {
            if self.irq_mode && !self.irq_masked { self.cq_seen = self.cq_tail; self.irqs += 1; self.irq_masked = true; }
        }
    }

    pub struct State {
        pub readless_required: bool,
        pub time_us: u64,
        pub caps: u32,
        pub sts: u32,
        pub dev_ctl_log: Vec<u32>,
        pub reg_writes: Vec<(usize, u32)>,
        pub resp: u64,
        pub aq_base: u64,
        pub acq_base: u64,
        pub aenq_base: u64,
        pub aenq_caps: u32,
        pub aq_proc: u16,
        pub acq_tail: u16,
        pub acq_phase: bool,
        pub aenq_tail: u16,
        pub aenq_phase: bool,
        pub aenq_head_db: u32,
        pub intr_mask: u32,
        pub cmds: Vec<(u8, u8)>,
        pub violations: Vec<String>,
        pub resets: u32,
        pub enabled_groups: u32,
        pub mtu: u32,
        pub tx: Q,
        pub rx: Q,
        pub frames: Vec<Vec<u8>>,
        pub loopback: bool,
        pub stall_tx: bool,
        pub rx_dropped: u32,
        pub readless_served: u32,
        pub fail_feature: Option<u8>,
        pub swallow_admin: bool,
        pub mem: Vec<(*mut u8, std::alloc::Layout)>,
        // ── LLQ (BAR2) ──
        pub llq_offered: bool,
        pub llq_max_queues: u32,
        pub llq_max_depth: u32,
        pub llq_loc: u16,
        pub llq_entry_sup: u16,
        pub llq_before_sup: u16,
        pub llq_stride_sup: u16,
        /// Enabled words the driver set: [loc, entry, before, stride].
        pub llq_set: Option<[u16; 4]>,
        pub llq_fail_create: bool,
        pub bar2: Vec<u8>,
        pub bar2_present: bool,
        /// BAR2 written since the last write barrier.
        pub unfenced: bool,
        pub llq_frames: usize,
        pub host_sq_frames: usize,
        pub sq_placements: Vec<(u8, u8)>,
        pub last_llq_off: usize,
        // ── MSI-X ──
        pub fail_irq_cq: bool,
        pub no_unmask_reg: bool,
        /// Interrupts delivered on table entry 0 (AENQ / admin).
        pub mgmt_irqs: u32,
        pub unmask_writes: u32,
    }

    pub const LLQ_DESC_OFF: usize = 0x2000;

    impl State {
        fn new(readless: bool) -> State {
            State {
                readless_required: readless, time_us: 0,
                // contiguous bit 0 = 0, reset timeout 1 (100 ms), 48-bit DMA, admin timeout 3 (300 ms)
                caps: 1 << 1 | 48 << 8 | 3 << 16,
                sts: STS_READY, dev_ctl_log: vec![], reg_writes: vec![], resp: 0, aq_base: 0, acq_base: 0,
                aenq_base: 0, aenq_caps: 0, aq_proc: 0, acq_tail: 0, acq_phase: true, aenq_tail: 0,
                aenq_phase: true, aenq_head_db: 0, intr_mask: 0, cmds: vec![], violations: vec![],
                resets: 0, enabled_groups: 0, mtu: 0, tx: Q::default(), rx: Q::default(), frames: vec![],
                loopback: true, stall_tx: false, rx_dropped: 0, readless_served: 0, fail_feature: None, swallow_admin: false,
                mem: vec![],
                llq_offered: true, llq_max_queues: 4, llq_max_depth: 1024, llq_loc: 1, llq_entry_sup: 0x7,
                llq_before_sup: 0x3, llq_stride_sup: 0x2, llq_set: None, llq_fail_create: false,
                bar2: vec![0u8; 0x2000 + 256 * 1024], bar2_present: true, unfenced: false, llq_frames: 0,
                host_sq_frames: 0, sq_placements: vec![], last_llq_off: 0,
                fail_irq_cq: false, no_unmask_reg: false, mgmt_irqs: 0, unmask_writes: 0,
            }
        }

        fn violation(&mut self, s: String) { self.violations.push(s); }

        fn plain_read(&mut self, off: usize) -> u32 {
            match off {
                reg::VERSION => 0x0200,
                reg::CONTROLLER_VERSION => 0x0001_0203,
                reg::CAPS => self.caps,
                reg::DEV_STS => self.sts,
                reg::ACQ_TAIL => self.acq_tail as u32,
                _ => 0,
            }
        }

        fn read(&mut self, off: usize) -> u32 {
            if self.readless_required {
                self.violation(format!("plain read of {:#x} on a readless device", off));
                return !0;
            }
            self.plain_read(off)
        }

        fn device_reset(&mut self) {
            self.resets += 1;
            self.sts = STS_RESET_IN_PROGRESS;
            // MMIO_RESP survives a reset (Linux programs it once per probe).
            self.aq_base = 0;
            self.acq_base = 0;
            self.aenq_base = 0;
            self.aq_proc = 0;
            self.acq_tail = 0;
            self.acq_phase = true;
            self.aenq_tail = 0;
            self.aenq_phase = true;
            self.aenq_head_db = 0;
            self.enabled_groups = 0;
            self.tx = Q::default();
            self.rx = Q::default();
            self.llq_set = None;
        }

        pub fn write(&mut self, off: usize, v: u32) {
            self.reg_writes.push((off, v));
            match off {
                reg::DEV_CTL => {
                    self.dev_ctl_log.push(v);
                    if v & CTL_RESET != 0 { self.device_reset(); }
                    else if self.sts & STS_RESET_IN_PROGRESS != 0 { self.sts = STS_READY | STS_RESET_FINISHED; }
                }
                reg::MMIO_RESP_LO => self.resp = (self.resp & !0xffff_ffff) | v as u64,
                reg::MMIO_RESP_HI => self.resp = (self.resp & 0xffff_ffff) | (v as u64) << 32,
                reg::MMIO_REG_READ => {
                    let (req, o) = (v as u16, (v >> 16) as usize);
                    if self.resp == 0 { self.violation("MMIO_REG_READ before MMIO_RESP".into()); return; }
                    let val = self.plain_read(o);
                    unsafe { w16(self.resp, 2, o as u16); w32(self.resp, 4, val); w16(self.resp, 0, req); }
                    self.readless_served += 1;
                }
                reg::AQ_BASE_LO => self.aq_base = (self.aq_base & !0xffff_ffff) | v as u64,
                reg::AQ_BASE_HI => self.aq_base = (self.aq_base & 0xffff_ffff) | (v as u64) << 32,
                reg::ACQ_BASE_LO => self.acq_base = (self.acq_base & !0xffff_ffff) | v as u64,
                reg::ACQ_BASE_HI => self.acq_base = (self.acq_base & 0xffff_ffff) | (v as u64) << 32,
                reg::AENQ_BASE_LO => self.aenq_base = (self.aenq_base & !0xffff_ffff) | v as u64,
                reg::AENQ_BASE_HI => self.aenq_base = (self.aenq_base & 0xffff_ffff) | (v as u64) << 32,
                reg::AENQ_CAPS => self.aenq_caps = v,
                reg::AENQ_HEAD_DB => self.aenq_head_db = v,
                reg::INTR_MASK => self.intr_mask = v,
                reg::AQ_DB => { if self.swallow_admin { self.aq_proc = v as u16; } else { self.process_admin(v as u16); } }
                TX_SQ_DB => {
                    if self.tx.llq_entry != 0 && self.unfenced {
                        self.violation("LLQ doorbell without a write barrier after the BAR2 stores".into());
                    }
                    self.tx.posted = v as u16; if !self.stall_tx { self.process_tx(); }
                }
                RX_SQ_DB => self.rx.posted = v as u16,
                TX_CQ_DB => self.tx.cq_head_db = v,
                RX_CQ_DB => self.rx.cq_head_db = v,
                TX_CQ_UNMASK | RX_CQ_UNMASK => {
                    self.unmask_writes += 1;
                    let q = if off == TX_CQ_UNMASK { &mut self.tx } else { &mut self.rx };
                    if v & INTR_UNMASK != 0 {
                        q.irq_masked = false;
                        // Work already queued fires at once (level of the real device's behaviour).
                        if q.cq_tail != q.cq_seen { q.fire(); }
                    }
                }
                _ => {}
            }
        }

        fn process_admin(&mut self, tail: u16) {
            while self.aq_proc != tail {
                let slot = (self.aq_proc & 31) as usize * 64;
                let (id, opcode, flags) = unsafe {
                    (r16(self.aq_base, slot), *p8(self.aq_base, slot + 2), *p8(self.aq_base, slot + 3))
                };
                let exp_phase = (self.aq_proc / 32) % 2 == 0;
                if (flags & 1 != 0) != exp_phase { self.violation(format!("admin SQ phase wrong at {}", self.aq_proc)); }
                self.aq_proc = self.aq_proc.wrapping_add(1);
                let mut status = 0u8;
                let mut resp = [0u8; 64];
                let b = self.aq_base;
                let feat = unsafe { *p8(b, slot + 17) };
                self.cmds.push((opcode, if opcode == op::GET_FEATURE || opcode == op::SET_FEATURE { feat } else { 0 }));
                let ctrl = unsafe { r32(b, slot + 8) as u64 | (r16(b, slot + 12) as u64) << 32 };
                match opcode {
                    op::GET_FEATURE => {
                        if self.fail_feature == Some(feat) { status = 4; }
                        else if feat == feat::DEVICE_ATTRIBUTES {
                            unsafe {
                                w32(ctrl, 0, 0x77); w32(ctrl, 16, 48);
                                for (i, m) in MAC.iter().enumerate() { w8(ctrl, 24 + i, *m); }
                                w32(ctrl, 32, 9001);
                            }
                        } else if feat == feat::MAX_QUEUES_EXT {
                            unsafe { w32(ctrl, 0, 1); w32(ctrl, 20, 1024); w32(ctrl, 28, 1024); w32(ctrl, 36, 96); }
                        } else if feat == feat::AENQ_CONFIG {
                            unsafe { w32(ctrl, 0, 0x1f); }
                        } else if feat == feat::LLQ {
                            if !self.llq_offered { status = 4; } else {
                                unsafe {
                                    w32(ctrl, 0, self.llq_max_queues); w32(ctrl, 4, self.llq_max_depth);
                                    w16(ctrl, 8, self.llq_loc); w16(ctrl, 12, self.llq_entry_sup);
                                    w16(ctrl, 16, self.llq_before_sup); w16(ctrl, 20, self.llq_stride_sup);
                                }
                            }
                        } else if feat != feat::MAX_QUEUES_NUM { status = 4; }
                    }
                    op::SET_FEATURE => {
                        let v = unsafe { r32(b, slot + 24) };
                        if feat == feat::AENQ_CONFIG { self.enabled_groups = v; }
                        if feat == feat::LLQ {
                            let h = |o: usize| unsafe { r16(b, slot + 20 + o) };
                            self.llq_set = Some([h(10), h(14), h(18), h(22)]);
                        }
                        if feat == feat::MTU { self.mtu = unsafe { r32(b, slot + 20) }; }
                    }
                    op::CREATE_CQ => {
                        let (fail_irq, no_unm) = (self.fail_irq_cq, self.no_unmask_reg);
                        let idx = if self.tx.created { 1u16 } else { 0 };
                        let q = if idx == 0 { &mut self.tx } else { &mut self.rx };
                        unsafe {
                            q.cq_words = *p8(b, slot + 5);
                            q.depth = r16(b, slot + 6);
                            q.cq_ba = r32(b, slot + 12) as u64 | (r16(b, slot + 16) as u64) << 32;
                        }
                        q.irq_mode = unsafe { *p8(b, slot + 4) } & (1 << 5) != 0;
                        q.msix_vector = unsafe { r32(b, slot + 8) };
                        q.irq_masked = true;
                        q.created = true;
                        q.cq_phase = true;
                        resp[8..10].copy_from_slice(&idx.to_le_bytes());
                        resp[10..12].copy_from_slice(&q.depth.to_le_bytes());
                        let db = if idx == 0 { TX_CQ_DB } else { RX_CQ_DB } as u32;
                        resp[16..20].copy_from_slice(&db.to_le_bytes());
                        if fail_irq && q.irq_mode { status = 3; q.created = false; }
                        let ub = if idx == 0 { TX_CQ_UNMASK } else { RX_CQ_UNMASK } as u32;
                        if !no_unm { resp[20..24].copy_from_slice(&ub.to_le_bytes()); }
                    }
                    op::CREATE_SQ => {
                        let dir = unsafe { *p8(b, slot + 4) } >> 5;
                        let placement = unsafe { *p8(b, slot + 6) } & 0xf;
                        self.sq_placements.push((dir, placement));
                        let llq_en = self.llq_set;
                        if placement == 3 {
                            if dir != 1 || self.llq_fail_create || llq_en.is_none() || !self.bar2_present { status = 3; }
                        } else if placement != 1 { status = 3; }
                        let q = if dir == 1 { &mut self.tx } else { &mut self.rx };
                        if placement == 3 && status == 0 {
                            q.llq_entry = match llq_en.unwrap()[1] { 1 => 128, 2 => 192, 4 => 256, _ => 0 };
                            if q.llq_entry == 0 { status = 3; }
                            resp[16..20].copy_from_slice(&(LLQ_DESC_OFF as u32).to_le_bytes());
                        }
                        unsafe {
                            q.sq_ba = r32(b, slot + 12) as u64 | (r16(b, slot + 16) as u64) << 32;
                            let d = r16(b, slot + 10);
                            if d != q.depth { status = 2; }
                        }
                        q.sq_phase = true;
                        let db = if dir == 1 { TX_SQ_DB } else { RX_SQ_DB } as u32;
                        resp[12..16].copy_from_slice(&db.to_le_bytes());
                    }
                    _ => {}
                }
                let cs = (self.acq_tail & 31) as usize * 64;
                resp[0..2].copy_from_slice(&id.to_le_bytes());
                resp[2] = status;
                resp[3] = self.acq_phase as u8;
                resp[6..8].copy_from_slice(&self.aq_proc.to_le_bytes());
                // Payload first, flags (the phase) last, like a DMA engine.
                unsafe {
                    for i in 4..64 { w8(self.acq_base, cs + i, resp[i]); }
                    w16(self.acq_base, cs, id);
                    w8(self.acq_base, cs + 2, status);
                    w8(self.acq_base, cs + 3, resp[3]);
                }
                self.acq_tail = self.acq_tail.wrapping_add(1);
                if self.acq_tail & 31 == 0 { self.acq_phase = !self.acq_phase; }
            }
        }

        fn process_tx(&mut self) {
            while self.tx.proc != self.tx.posted {
                if self.tx.llq_entry != 0 { self.process_tx_llq(); continue; }
                self.host_sq_frames += 1;
                let q = &mut self.tx;
                let d = (q.proc & (q.depth - 1)) as usize * 16;
                let (w0, w1, lo, hi) = unsafe { (r32(q.sq_ba, d), r32(q.sq_ba, d + 4), r32(q.sq_ba, d + 8), r32(q.sq_ba, d + 12)) };
                if ((w0 >> 24) & 1 != 0) != q.sq_phase { self.violations.push(format!("TX SQ phase wrong at {}", q.proc)); }
                let len = (w0 & 0xffff) as usize;
                let req = ((w0 >> 16) & 0x3f) << 10 | (w1 >> 22);
                let pa = lo as u64 | ((hi & 0xffff) as u64) << 32;
                let mut f = vec![0u8; len];
                unsafe { std::ptr::copy_nonoverlapping(pa as *const u8, f.as_mut_ptr(), len); }
                q.proc = q.proc.wrapping_add(1);
                if q.proc & (q.depth - 1) == 0 { q.sq_phase = !q.sq_phase; }
                let c = (q.cq_tail & (q.depth - 1)) as usize * 8;
                unsafe {
                    w16(q.cq_ba, c, req as u16);
                    w8(q.cq_ba, c + 2, 0);
                    w16(q.cq_ba, c + 6, q.proc);
                    w8(q.cq_ba, c + 3, q.cq_phase as u8);
                }
                q.cq_tail = q.cq_tail.wrapping_add(1);
                if q.cq_tail & (q.depth - 1) == 0 { q.cq_phase = !q.cq_phase; }
                q.fire();
                if self.loopback { self.inject_rx(&f); }
                self.frames.push(f);
            }
        }

        /// Decode one LLQ entry from BAR2: descriptors, then the pushed header;
        /// the rest of the frame is read from the host buffer the packet
        /// descriptor points at (after the pushed header).
        fn process_tx_llq(&mut self) {
            let q = &mut self.tx;
            let esz = q.llq_entry;
            let before = (self.llq_set.unwrap()[2]) as usize;
            let off = LLQ_DESC_OFF + (q.proc & (q.depth - 1)) as usize * esz;
            self.last_llq_off = off;
            let e: Vec<u8> = self.bar2[off..off + esz].to_vec();
            let w = |o: usize| u32::from_le_bytes([e[o], e[o + 1], e[o + 2], e[o + 3]]);
            let mut pkt = None;
            let mut metas = 0;
            for k in 0..before {
                let w0 = w(k * 16);
                if (w0 >> 23) & 1 != 0 { metas += 1; continue; }
                if (w0 >> 26) & 1 != 0 { pkt = Some(k); break; }
            }
            let k = match pkt { Some(k) => k, None => { self.violations.push(format!("no packet descriptor in LLQ entry {}", q.proc)); q.proc = q.proc.wrapping_add(1); return; } };
            let _ = metas;
            let w0 = w(k * 16);
            if ((w0 >> 24) & 1 != 0) != q.sq_phase { self.violations.push(format!("LLQ phase wrong at {}", q.proc)); }
            if w0 >> 26 & 1 == 0 || w0 >> 27 & 1 == 0 { self.violations.push("LLQ pkt desc not first+last".into()); }
            let len = (w0 & 0xffff) as usize;
            let w1 = w(k * 16 + 4);
            let (lo, hi3) = (w(k * 16 + 8), w(k * 16 + 12));
            let hl = (hi3 >> 24) as usize;
            let pa = lo as u64 | ((hi3 & 0xffff) as u64) << 32;
            let req = ((w0 >> 16) & 0x3f) << 10 | (w1 >> 22);
            if hl > len || before * 16 + hl > esz { self.violations.push(format!("LLQ header_length {} bad (len {})", hl, len)); }
            let mut f = vec![0u8; len];
            f[..hl.min(len)].copy_from_slice(&e[before * 16..before * 16 + hl.min(len)]);
            if len > hl { unsafe { std::ptr::copy_nonoverlapping(pa as *const u8, f.as_mut_ptr().add(hl), len - hl); } }
            q.proc = q.proc.wrapping_add(1);
            if q.proc & (q.depth - 1) == 0 { q.sq_phase = !q.sq_phase; }
            let c = (q.cq_tail & (q.depth - 1)) as usize * 8;
            unsafe {
                w16(q.cq_ba, c, req as u16);
                w8(q.cq_ba, c + 2, 0);
                w16(q.cq_ba, c + 6, q.proc);
                w8(q.cq_ba, c + 3, q.cq_phase as u8);
            }
            q.cq_tail = q.cq_tail.wrapping_add(1);
            if q.cq_tail & (q.depth - 1) == 0 { q.cq_phase = !q.cq_phase; }
            q.fire();
            self.llq_frames += 1;
            if self.loopback { self.inject_rx(&f); }
            self.frames.push(f);
        }

        /// Deliver a frame into the next posted RX buffer.
        pub fn inject_rx(&mut self, frame: &[u8]) -> bool {
            let q = &mut self.rx;
            if q.proc == q.posted { self.rx_dropped += 1; return false; }
            let d = (q.proc & (q.depth - 1)) as usize * 16;
            let (ctrl, id, lo, hi) = unsafe { (*p8(q.sq_ba, d + 3), r16(q.sq_ba, d + 4), r32(q.sq_ba, d + 8), r16(q.sq_ba, d + 12)) };
            if (ctrl & 1 != 0) != q.sq_phase { self.violations.push(format!("RX SQ phase wrong at {}", q.proc)); }
            let pa = lo as u64 | (hi as u64) << 32;
            unsafe { std::ptr::copy_nonoverlapping(frame.as_ptr(), pa as *mut u8, frame.len()); }
            q.proc = q.proc.wrapping_add(1);
            if q.proc & (q.depth - 1) == 0 { q.sq_phase = !q.sq_phase; }
            let c = (q.cq_tail & (q.depth - 1)) as usize * 16;
            unsafe {
                w16(q.cq_ba, c + 4, frame.len() as u16);
                w16(q.cq_ba, c + 6, id);
                w32(q.cq_ba, c, (q.cq_phase as u32) << 24 | 1 << 26 | 1 << 27);
            }
            q.cq_tail = q.cq_tail.wrapping_add(1);
            if q.cq_tail & (q.depth - 1) == 0 { q.cq_phase = !q.cq_phase; }
            q.fire();
            true
        }

        pub fn push_aenq(&mut self, group: u16, payload0: u32) {
            let s = (self.aenq_tail & 31) as usize * 64;
            let b = self.aenq_base;
            unsafe {
                w16(b, s, group);
                w32(b, s + 16, payload0);
                w8(b, s + 4, self.aenq_phase as u8);
            }
            self.aenq_tail = self.aenq_tail.wrapping_add(1);
            if self.aenq_tail & 31 == 0 { self.aenq_phase = !self.aenq_phase; }
            if self.intr_mask & INTR_MASK_ADMIN == 0 { self.mgmt_irqs += 1; }
        }

        pub fn release_tx(&mut self) { self.stall_tx = false; self.process_tx(); }
    }

    impl Drop for State {
        fn drop(&mut self) { for (p, l) in self.mem.drain(..) { unsafe { std::alloc::dealloc(p, l) } } }
    }

    #[derive(Clone)]
    pub struct MockBus(pub Rc<RefCell<State>>);

    impl MockBus {
        pub fn new(readless: bool) -> MockBus { MockBus(Rc::new(RefCell::new(State::new(readless)))) }
        pub fn advance_ms(&self, ms: u64) { self.0.borrow_mut().time_us += ms * 1000; }
        pub fn st(&self) -> std::cell::RefMut<'_, State> { self.0.borrow_mut() }
    }

    impl EnaBus for MockBus {
        fn read32(&self, off: usize) -> u32 { self.0.borrow_mut().read(off) }
        fn write32(&self, off: usize, v: u32) { self.0.borrow_mut().write(off, v) }
        fn dma_alloc(&self, len: usize) -> Option<DmaBuf> {
            assert!(len <= 4096);
            let l = std::alloc::Layout::from_size_align(4096, 4096).unwrap();
            let p = unsafe { std::alloc::alloc_zeroed(l) };
            self.0.borrow_mut().mem.push((p, l));
            Some(DmaBuf { va: p, pa: p as u64, len: 4096 })
        }
        fn udelay(&self, us: u64) { self.0.borrow_mut().time_us += us; }
        fn now_ms(&self) -> u64 { self.0.borrow().time_us / 1000 }
        fn llq_bar_len(&self) -> usize { let s = self.0.borrow(); if s.bar2_present { s.bar2.len() } else { 0 } }
        fn llq_write(&self, off: usize, data: &[u8]) {
            let mut s = self.0.borrow_mut();
            s.bar2[off..off + data.len()].copy_from_slice(data);
            s.unfenced = true;
        }
        fn wmb(&self) { self.0.borrow_mut().unfenced = false; }
    }
}

#[cfg(test)]
mod tests {
    use super::ena_hw::*;
    use super::mock::*;

    fn up(readless: bool) -> (MockBus, Ena<MockBus>) {
        let bus = MockBus::new(readless);
        let mut ena = Ena::new(bus.clone(), readless).unwrap();
        ena.init().expect("init");
        (bus, ena)
    }

    fn frame(n: usize, seed: u32) -> Vec<u8> {
        let len = 60 + (n * 37 + seed as usize) % 1400;
        (0..len).map(|i| (i as u32 ^ seed ^ (n as u32).wrapping_mul(2654435761)) as u8).collect()
    }

    #[test]
    fn init_order_and_facts() {
        let (bus, ena) = up(false);
        let st = bus.st();
        assert!(st.violations.is_empty(), "{:?}", st.violations);
        // Reset precedes every admin-queue register write; the first command
        // only comes after the AENQ head doorbell hands the device its ring.
        let pos = |off: usize| st.reg_writes.iter().position(|w| w.0 == off).unwrap();
        assert!(st.reg_writes[0] == (reg::DEV_CTL, CTL_RESET));
        assert_eq!(st.reg_writes[1], (reg::DEV_CTL, 0));
        assert!(pos(reg::DEV_CTL) < pos(reg::AQ_BASE_LO));
        assert!(pos(reg::AQ_CAPS) < pos(reg::ACQ_CAPS) && pos(reg::ACQ_CAPS) < pos(reg::AENQ_CAPS));
        assert!(pos(reg::AENQ_CAPS) < pos(reg::INTR_MASK) && pos(reg::INTR_MASK) < pos(reg::AENQ_HEAD_DB));
        assert!(pos(reg::AENQ_HEAD_DB) < pos(reg::AQ_DB));
        assert_eq!(st.intr_mask, INTR_MASK_ADMIN);
        let aenq_db = st.reg_writes.iter().find(|w| w.0 == reg::AENQ_HEAD_DB).unwrap().1;
        assert_eq!(aenq_db, AENQ_DEPTH as u32);
        // Admin sequence.
        let ops: Vec<(u8, u8)> = st.cmds.clone();
        let expect = [
            (op::GET_FEATURE, feat::DEVICE_ATTRIBUTES), (op::GET_FEATURE, feat::MAX_QUEUES_EXT),
            (op::GET_FEATURE, feat::LLQ), (op::SET_FEATURE, feat::LLQ), (op::GET_FEATURE, feat::AENQ_CONFIG),
            (op::SET_FEATURE, feat::AENQ_CONFIG), (op::SET_FEATURE, feat::MTU),
            (op::CREATE_CQ, 0), (op::CREATE_SQ, 0), (op::CREATE_CQ, 0), (op::CREATE_SQ, 0),
        ];
        assert_eq!(ops, expect);
        assert_eq!(st.enabled_groups, 0x1f);
        assert_eq!(st.mtu, 1500);
        // Queue geometry, placement host, polled CQs.
        assert_eq!((st.tx.depth, st.rx.depth), (TX_DEPTH, RX_DEPTH));
        assert_eq!((st.tx.cq_words, st.rx.cq_words), (2, 4));
        drop(st);
        assert_eq!(ena.mac, MAC);
        assert_eq!(ena.info.max_mtu, 9001);
        assert_eq!(ena.info.dma_width, 48);
        assert_eq!(ena.info.llq.unwrap()[1], 1024);
        assert!(ena.link_up());
        assert_eq!(ena.link_gen(), 1);
    }

    #[test]
    fn legacy_queue_descriptor_fallback() {
        let bus = MockBus::new(false);
        bus.st().fail_feature = Some(feat::MAX_QUEUES_EXT);
        let mut ena = Ena::new(bus.clone(), false).unwrap();
        ena.init().expect("init falls back to MAX_QUEUES_NUM");
        assert!(bus.st().cmds.contains(&(op::GET_FEATURE, feat::MAX_QUEUES_NUM)));
    }

    #[test]
    fn readless_reads_never_touch_registers() {
        let (bus, mut ena) = up(true);
        assert!(bus.st().violations.is_empty(), "{:?}", bus.st().violations);
        assert!(bus.st().readless_served >= 4, "version/caps/sts via MMIO_REG_READ");
        assert_eq!(ena.rd(reg::VERSION), 0x0200);
        assert_eq!(ena.info.version, 0x0200);
        assert_eq!(ena.stats.readless_timeouts, 0);
        // The response address was programmed before the first read.
        let st = bus.st();
        let resp_w = st.reg_writes.iter().position(|w| w.0 == reg::MMIO_RESP_LO).unwrap();
        let first_read = st.reg_writes.iter().position(|w| w.0 == reg::MMIO_REG_READ).unwrap();
        assert!(resp_w < first_read);
    }

    #[test]
    fn plain_reads_on_a_readless_device_are_rejected() {
        // Driver told the device is not readless: the mock flags the reads and
        // returns all-ones, so bring-up fails instead of misreading.
        let bus = MockBus::new(true);
        let mut ena = Ena::new(bus.clone(), false).unwrap();
        assert!(ena.init().is_err());
        assert!(!bus.st().violations.is_empty());
    }

    #[test]
    fn readless_survives_reset_and_seq_wrap() {
        let (bus, mut ena) = up(true);
        for _ in 0..70_000u32 { assert_eq!(ena.rd(reg::CAPS) & 0xff00, 48 << 8); }
        ena.dev_reset(reset_reason::NORMAL).unwrap();
        assert_eq!(ena.rd(reg::VERSION), 0x0200);
        assert!(bus.st().violations.is_empty());
    }

    #[test]
    fn admin_phase_wraps() {
        let (bus, mut ena) = up(false);
        for i in 0..200u32 { ena.set_mtu(1400 + i % 100).expect("admin cmd"); }
        let st = bus.st();
        assert!(st.violations.is_empty(), "{:?}", st.violations);
        assert_eq!(st.mtu, 1400 + 199 % 100);
        assert!(ena.stats.admin_cmds > 200);
    }

    #[test]
    fn admin_timeout_is_an_error_not_a_hang() {
        let (bus, mut ena) = up(false);
        bus.st().swallow_admin = true;
        assert!(ena.set_mtu(1500).is_err());
        assert_eq!(ena.stats.admin_timeouts, 1);
        // The watchdog treats a dead admin queue as a reset reason.
        bus.st().swallow_admin = false;
        assert_eq!(ena.watchdog(), Watchdog::Reset("admin queue timeout"));
        assert!(ena.set_mtu(1500).is_ok());
    }

    #[test]
    fn loopback_10k_frames_with_ring_wrap() {
        let (bus, mut ena) = up(false);
        let mut buf = vec![0u8; 2048];
        for n in 0..10_000usize {
            let f = frame(n, 7);
            assert!(ena.send(&f), "send {} failed", n);
            let got = ena.recv(&mut buf).unwrap_or_else(|| panic!("no frame {}", n));
            let want = if f.len() < MIN_FRAME { MIN_FRAME } else { f.len() };
            assert_eq!(got, want);
            assert_eq!(&buf[..f.len()], &f[..], "frame {} corrupt", n);
        }
        assert!(ena.recv(&mut buf).is_none());
        let st = bus.st();
        assert!(st.violations.is_empty(), "{:?}", st.violations);
        assert_eq!(st.frames.len(), 10_000);
        assert_eq!(st.rx_dropped, 0);
        drop(st);
        assert_eq!(ena.stats.tx_pkts, 10_000);
        assert_eq!(ena.stats.rx_pkts, 10_000);
    }

    #[test]
    fn burst_fills_rings_then_drains() {
        let (bus, mut ena) = up(false);
        let mut buf = vec![0u8; 2048];
        let mut sent = 0;
        let mut recvd = 0;
        for round in 0..200usize {
            for k in 0..100usize {
                let f = frame(round * 100 + k, 3);
                if ena.send(&f) { sent += 1; }
            }
            while let Some(n) = ena.recv(&mut buf) { assert!(n >= 60); recvd += 1; }
        }
        assert_eq!(sent, recvd);
        assert!(bus.st().violations.is_empty());
    }

    #[test]
    fn tx_ring_full_backpressure() {
        let (bus, mut ena) = up(false);
        bus.st().stall_tx = true;
        bus.st().loopback = false;
        let f = frame(1, 1);
        let mut n = 0;
        while ena.send(&f) { n += 1; assert!(n <= TX_DEPTH as usize); }
        assert_eq!(n, TX_DEPTH as usize);
        assert!(!ena.tx_ready());
        assert!(ena.stats.tx_full >= 1);
        bus.st().release_tx();
        assert!(ena.tx_ready());
        assert!(ena.send(&f));
        assert!(bus.st().violations.is_empty());
    }

    #[test]
    fn oversized_and_empty_frames_refused() {
        let (_bus, mut ena) = up(false);
        assert!(!ena.send(&[]));
        assert!(!ena.send(&vec![0u8; SLOT + 1]));
        assert!(ena.send(&vec![1u8; 1514]));
    }

    #[test]
    fn rx_without_posted_buffers_drops() {
        let (bus, mut ena) = up(false);
        bus.st().loopback = false;
        let mut buf = vec![0u8; 2048];
        // Fill the device's RX slots (depth-1 posted), then one more drops.
        let mut ok = 0;
        while bus.st().inject_rx(&frame(ok, 5)) { ok += 1; assert!(ok < 1000); }
        assert_eq!(ok, RX_DEPTH as usize - 1);
        assert_eq!(bus.st().rx_dropped, 1);
        for i in 0..ok {
            let n = ena.recv(&mut buf).expect("queued frame");
            assert_eq!(&buf[..n], &frame(i, 5)[..n]);
        }
        assert!(ena.recv(&mut buf).is_none());
        // Slots were recycled: another full round works.
        for i in 0..50 { assert!(bus.st().inject_rx(&frame(i, 6))); }
        for _ in 0..50 { assert!(ena.recv(&mut buf).is_some()); }
    }

    #[test]
    fn link_change_bumps_link_gen_on_up_only() {
        let (bus, mut ena) = up(false);
        let g0 = ena.link_gen();
        bus.st().push_aenq(aenq::LINK_CHANGE, 0);
        assert_eq!(ena.poll_aenq(), 1);
        assert!(!ena.link_up());
        assert_eq!(ena.link_gen(), g0);
        bus.st().push_aenq(aenq::LINK_CHANGE, 1);
        assert_eq!(ena.poll_aenq(), 1);
        assert!(ena.link_up());
        assert_eq!(ena.link_gen(), g0 + 1);
        // A repeated "up" is not a transition.
        bus.st().push_aenq(aenq::LINK_CHANGE, 1);
        ena.poll_aenq();
        assert_eq!(ena.link_gen(), g0 + 1);
        // The head doorbell follows consumption.
        let db = bus.st().aenq_head_db;
        let tail = bus.st().aenq_tail as u32;
        assert_eq!(db, tail);
    }

    #[test]
    fn aenq_phase_wraps() {
        let (bus, mut ena) = up(false);
        for i in 0..150u32 {
            bus.st().push_aenq(aenq::NOTIFICATION, i);
            assert_eq!(ena.poll_aenq(), 1, "event {}", i);
        }
        assert_eq!(ena.stats.aenq_events, 150);
        assert_eq!(bus.st().aenq_head_db, 150);
    }

    #[test]
    fn keepalives_keep_the_watchdog_quiet() {
        let (bus, mut ena) = up(false);
        for _ in 0..30 {
            bus.advance_ms(1000);
            bus.st().push_aenq(aenq::KEEP_ALIVE, 0);
            assert_eq!(ena.watchdog(), Watchdog::Ok);
        }
        assert_eq!(bus.st().resets, 1, "only the init reset");
        assert_eq!(ena.stats.keepalives, 30);
    }

    #[test]
    fn missing_keepalive_resets_and_recovers() {
        let (bus, mut ena) = up(false);
        let g = ena.link_gen();
        bus.advance_ms(5000);
        assert_eq!(ena.watchdog(), Watchdog::Ok);
        bus.advance_ms(2000);
        assert_eq!(ena.watchdog(), Watchdog::Reset("keep-alive timeout"));
        let st = bus.st();
        assert_eq!(st.resets, 2);
        let reasons: Vec<u32> = st.dev_ctl_log.iter().filter(|v| *v & CTL_RESET != 0).map(|v| v >> CTL_RESET_REASON_SHIFT).collect();
        assert_eq!(reasons, vec![reset_reason::NORMAL, reset_reason::KEEP_ALIVE_TO]);
        drop(st);
        assert!(ena.link_gen() > g, "DHCP must restart");
        assert!(ena.running());
        // The data path works again on the rebuilt queues.
        let mut buf = vec![0u8; 2048];
        for n in 0..500 { let f = frame(n, 9); assert!(ena.send(&f)); assert_eq!(ena.recv(&mut buf).unwrap(), f.len().max(60)); }
        assert!(bus.st().violations.is_empty(), "{:?}", bus.st().violations);
    }

    #[test]
    fn fatal_status_resets() {
        let (bus, mut ena) = up(false);
        bus.st().sts |= STS_FATAL;
        let w = ena.watchdog();
        assert_eq!(w, Watchdog::Reset("fatal error"));
        assert_eq!(bus.st().sts & STS_FATAL, 0);
        assert!(ena.running());
    }

    #[test]
    fn fatal_aenq_resets() {
        let (bus, mut ena) = up(false);
        bus.st().push_aenq(aenq::FATAL_ERROR, 0);
        assert_eq!(ena.watchdog(), Watchdog::Reset("fatal error"));
    }

    #[test]
    fn failed_reinit_is_retried_every_second() {
        let (bus, mut ena) = up(false);
        bus.st().fail_feature = Some(feat::DEVICE_ATTRIBUTES);
        bus.advance_ms(7000);
        assert_eq!(ena.watchdog(), Watchdog::ResetFailed("keep-alive timeout"));
        assert!(!ena.running());
        assert!(!ena.send(&frame(0, 0)));
        let mut buf = [0u8; 2048];
        assert!(ena.recv(&mut buf).is_none());
        bus.advance_ms(200);
        assert_eq!(ena.watchdog(), Watchdog::Ok, "backoff");
        bus.advance_ms(900);
        assert_eq!(ena.watchdog(), Watchdog::ResetFailed("retry"));
        bus.st().fail_feature = None;
        bus.advance_ms(1100);
        assert_eq!(ena.watchdog(), Watchdog::Reset("retry"));
        assert!(ena.running());
    }

    #[test]
    fn shutdown_resets_with_reason() {
        let (bus, mut ena) = up(false);
        ena.shutdown().unwrap();
        assert!(!ena.running());
        let last = bus.st().dev_ctl_log.iter().rev().find(|v| *v & CTL_RESET != 0).copied().unwrap();
        assert_eq!(last >> CTL_RESET_REASON_SHIFT, reset_reason::SHUTDOWN);
        assert!(!ena.send(&frame(0, 0)));
    }


    // ── LLQ ─────────────────────────────────────────────────────────────────

    fn caps(entry: u16, before: u16, stride: u16, loc: u16) -> LlqCaps {
        LlqCaps { max_queues: 4, max_depth: 1024, loc, entry, before, stride }
    }

    #[test]
    fn llq_negotiation_matrix() {
        // Everything offered: 128 B, 2 descs before the header, multi-desc stride.
        let c = llq_negotiate(&caps(7, 0xf, 3, 1)).unwrap();
        assert_eq!((c.entry_size, c.before, c.stride_multi), (128, 2, true));
        assert_eq!(c.header_space(), 128 - 2 * 16);
        // 128 B missing: 256 preferred over 192.
        assert_eq!(llq_negotiate(&caps(6, 2, 2, 1)).unwrap().entry_size, 256);
        assert_eq!(llq_negotiate(&caps(2, 2, 2, 1)).unwrap().entry_size, 192);
        assert_eq!(llq_negotiate(&caps(4, 2, 2, 1)).unwrap().entry_size, 256);
        // descs-before-header: 2 preferred, then 1, 4, 8.
        assert_eq!(llq_negotiate(&caps(1, 1, 2, 1)).unwrap().before, 1);
        assert_eq!(llq_negotiate(&caps(1, 0xc, 2, 1)).unwrap().before, 4);
        assert_eq!(llq_negotiate(&caps(4, 8, 2, 1)).unwrap().before, 8);
        // Stride: single only is accepted.
        assert!(!llq_negotiate(&caps(1, 2, 1, 1)).unwrap().stride_multi);
        // No usable option in a mandatory dimension: no LLQ.
        assert!(llq_negotiate(&caps(0, 2, 2, 1)).is_none(), "no entry size");
        assert!(llq_negotiate(&caps(1, 0, 2, 1)).is_none(), "no descs-before-header");
        assert!(llq_negotiate(&caps(1, 2, 0, 1)).is_none(), "no stride mode");
        assert!(llq_negotiate(&caps(1, 2, 2, 2)).is_none(), "separate-buffer header only");
        assert!(llq_negotiate(&caps(1, 2, 2, 0)).is_none(), "no header location");
        let mut z = caps(1, 2, 2, 1);
        z.max_queues = 0;
        assert!(llq_negotiate(&z).is_none());
        z = caps(1, 2, 2, 1);
        z.max_depth = 0;
        assert!(llq_negotiate(&z).is_none());
        // 256 B entry: bigger header window.
        assert_eq!(llq_negotiate(&caps(4, 2, 2, 1)).unwrap().header_space(), 256 - 32);
    }

    #[test]
    fn llq_bringup_uses_device_placement() {
        let (bus, ena) = up(false);
        let st = bus.st();
        assert!(st.violations.is_empty(), "{:?}", st.violations);
        assert!(ena.llq_active());
        // TX SQ device placement (3), RX SQ stays host (1).
        assert_eq!(st.sq_placements, vec![(1, 3), (2, 1)]);
        let en = st.llq_set.unwrap();
        assert_eq!(en, [1, 1, 2, 2], "inline header, 128 B, 2 descs before header, multi-desc stride");
        assert_eq!(st.tx.llq_entry, 128);
        let c = ena.info.llq_cfg.unwrap();
        assert_eq!((c.entry_size, c.before), (128, 2));
    }

    #[test]
    fn llq_entry_layout() {
        let (bus, mut ena) = up(false);
        let f: Vec<u8> = (0..300u32).map(|i| (i * 7 + 3) as u8).collect();
        assert!(ena.send(&f));
        let st = bus.st();
        assert!(st.violations.is_empty(), "{:?}", st.violations);
        assert_eq!(st.last_llq_off, LLQ_DESC_OFF, "first entry at the descriptors offset");
        let e = &st.bar2[LLQ_DESC_OFF..LLQ_DESC_OFF + 128];
        let w = |o: usize| u32::from_le_bytes([e[o], e[o + 1], e[o + 2], e[o + 3]]);
        // desc 0: meta descriptor (meta bit 23, phase 24).
        assert_eq!((w(0) >> 23) & 1, 1);
        assert_eq!((w(0) >> 24) & 1, 1);
        // desc 1: the packet: total length, phase, first+last+comp_req.
        assert_eq!(w(16) & 0xffff, 300);
        assert_eq!((w(16) >> 23) & 1, 0);
        assert_eq!((w(16) >> 24) & 7, 1 | 1 << 2);
        assert_eq!((w(16) >> 26) & 7, 7);
        // header_length = 96 bytes pushed after the two descriptors.
        assert_eq!(w(28) >> 24, 96);
        assert_eq!(&e[32..128], &f[..96]);
        // The remainder comes from the host buffer.
        let pa = w(24) as u64 | ((w(28) & 0xffff) as u64) << 32;
        let rest = unsafe { std::slice::from_raw_parts(pa as *const u8, 300 - 96) };
        assert_eq!(rest, &f[96..]);
        assert_eq!(st.tx.posted, 1);
        assert_eq!(st.frames[0], f);
    }

    #[test]
    fn llq_short_frame_is_fully_pushed() {
        let (bus, mut ena) = up(false);
        assert!(ena.send(&[0xab; 40]));
        let st = bus.st();
        assert!(st.violations.is_empty(), "{:?}", st.violations);
        let e = &st.bar2[LLQ_DESC_OFF..LLQ_DESC_OFF + 128];
        let w = |o: usize| u32::from_le_bytes([e[o], e[o + 1], e[o + 2], e[o + 3]]);
        assert_eq!(w(16) & 0xffff, 60, "padded to the minimum frame");
        assert_eq!(w(28) >> 24, 60);
        assert_eq!(st.frames[0].len(), 60);
        assert_eq!(&st.frames[0][..40], &[0xab; 40]);
    }

    #[test]
    fn llq_256_byte_entries() {
        let bus = MockBus::new(false);
        bus.st().llq_entry_sup = 4;
        let mut ena = Ena::new(bus.clone(), false).unwrap();
        ena.init().unwrap();
        assert!(ena.llq_active());
        assert_eq!(bus.st().tx.llq_entry, 256);
        let f = frame(5, 5);
        assert!(ena.send(&f));
        let mut buf = vec![0u8; 2048];
        assert_eq!(ena.recv(&mut buf).unwrap(), f.len());
        assert_eq!(&buf[..f.len()], &f[..]);
        assert!(bus.st().violations.is_empty(), "{:?}", bus.st().violations);
    }

    #[test]
    fn llq_loopback_10k_frames_with_ring_wrap() {
        let (bus, mut ena) = up(false);
        assert!(ena.llq_active());
        let mut buf = vec![0u8; 2048];
        for n in 0..10_000usize {
            let f = frame(n, 11);
            assert!(ena.send(&f), "send {} failed", n);
            let got = ena.recv(&mut buf).unwrap_or_else(|| panic!("no frame {}", n));
            assert_eq!(got, f.len().max(MIN_FRAME));
            assert_eq!(&buf[..f.len()], &f[..], "frame {} corrupt", n);
        }
        let st = bus.st();
        assert!(st.violations.is_empty(), "{:?}", st.violations);
        assert_eq!(st.llq_frames, 10_000);
        assert_eq!(st.host_sq_frames, 0);
        assert!(st.frames.len() == 10_000 && st.rx_dropped == 0);
    }

    #[test]
    fn llq_burst_fills_ring_then_drains() {
        let (bus, mut ena) = up(false);
        bus.st().stall_tx = true;
        bus.st().loopback = false;
        let f = frame(1, 1);
        let mut n = 0;
        while ena.send(&f) { n += 1; assert!(n <= TX_DEPTH as usize); }
        assert_eq!(n, TX_DEPTH as usize);
        bus.st().release_tx();
        assert!(ena.tx_ready());
        // Wrap the LLQ ring several times in bursts.
        bus.st().loopback = true;
        let mut buf = vec![0u8; 2048];
        for round in 0..100usize {
            let mut sent = 0;
            for k in 0..100usize { if ena.send(&frame(round * 100 + k, 2)) { sent += 1; } }
            let mut got = 0;
            while ena.recv(&mut buf).is_some() { got += 1; }
            assert_eq!(sent, got);
        }
        assert!(bus.st().violations.is_empty(), "{:?}", bus.st().violations);
    }

    #[test]
    fn llq_writes_are_fenced_before_the_doorbell() {
        let (bus, mut ena) = up(false);
        for n in 0..50 { assert!(ena.send(&frame(n, 1))); }
        assert!(!bus.st().unfenced);
        assert!(bus.st().violations.is_empty(), "{:?}", bus.st().violations);
    }

    #[test]
    fn llq_survives_reset() {
        let (bus, mut ena) = up(false);
        bus.advance_ms(7000);
        assert_eq!(ena.watchdog(), Watchdog::Reset("keep-alive timeout"));
        assert!(ena.llq_active());
        let mut buf = vec![0u8; 2048];
        for n in 0..400 { let f = frame(n, 4); assert!(ena.send(&f)); assert_eq!(ena.recv(&mut buf).unwrap(), f.len().max(60)); }
        assert!(bus.st().violations.is_empty(), "{:?}", bus.st().violations);
    }

    #[test]
    fn llq_depth_is_clamped_to_device_max() {
        let bus = MockBus::new(false);
        bus.st().llq_max_depth = 64;
        let mut ena = Ena::new(bus.clone(), false).unwrap();
        ena.init().unwrap();
        assert!(ena.llq_active());
        assert_eq!(bus.st().tx.depth, 64);
        let mut buf = vec![0u8; 2048];
        for n in 0..500 { let f = frame(n, 8); assert!(ena.send(&f)); assert!(ena.recv(&mut buf).is_some()); }
        assert!(bus.st().violations.is_empty(), "{:?}", bus.st().violations);
    }

    fn host_mode_works(bus: &MockBus, ena: &mut Ena<MockBus>) {
        assert!(!ena.llq_active());
        assert_eq!(bus.st().sq_placements.last().copied(), Some((2, 1)));
        assert!(bus.st().sq_placements.iter().all(|p| p.1 == 1));
        let mut buf = vec![0u8; 2048];
        for n in 0..10_000usize {
            let f = frame(n, 13);
            assert!(ena.send(&f));
            assert_eq!(ena.recv(&mut buf).unwrap(), f.len().max(MIN_FRAME));
            assert_eq!(&buf[..f.len()], &f[..]);
        }
        let st = bus.st();
        assert!(st.violations.is_empty(), "{:?}", st.violations);
        assert_eq!((st.llq_frames, st.host_sq_frames), (0, 10_000));
    }

    #[test]
    fn fallback_device_without_llq() {
        let bus = MockBus::new(false);
        bus.st().llq_offered = false;
        let mut ena = Ena::new(bus.clone(), false).unwrap();
        ena.init().unwrap();
        assert!(ena.info.llq.is_none());
        host_mode_works(&bus, &mut ena);
    }

    #[test]
    fn fallback_llq_disabled_by_knob() {
        let bus = MockBus::new(false);
        let mut ena = Ena::new(bus.clone(), false).unwrap();
        ena.set_llq_allowed(false);
        ena.init().unwrap();
        assert!(!bus.st().cmds.contains(&(op::SET_FEATURE, feat::LLQ)));
        host_mode_works(&bus, &mut ena);
    }

    #[test]
    fn fallback_no_bar2() {
        let bus = MockBus::new(false);
        bus.st().bar2_present = false;
        let mut ena = Ena::new(bus.clone(), false).unwrap();
        ena.init().unwrap();
        host_mode_works(&bus, &mut ena);
    }

    #[test]
    fn fallback_unusable_caps() {
        let bus = MockBus::new(false);
        bus.st().llq_loc = 2; // header in a separate buffer only
        let mut ena = Ena::new(bus.clone(), false).unwrap();
        ena.init().unwrap();
        host_mode_works(&bus, &mut ena);
    }

    #[test]
    fn fallback_when_device_refuses_dev_sq() {
        let bus = MockBus::new(false);
        bus.st().llq_fail_create = true;
        let mut ena = Ena::new(bus.clone(), false).unwrap();
        ena.init().expect("falls back to host placement after the refusal");
        assert!(bus.st().sq_placements.contains(&(1, 3)), "LLQ was attempted first");
        assert!(!ena.llq_active());
        let mut buf = vec![0u8; 2048];
        for n in 0..300 { let f = frame(n, 2); assert!(ena.send(&f)); assert!(ena.recv(&mut buf).is_some()); }
        assert_eq!(bus.st().llq_frames, 0);
        assert!(bus.st().violations.is_empty(), "{:?}", bus.st().violations);
    }

    #[test]
    fn device_ids() {
        assert_eq!(VENDOR, 0x1d0f);
        assert!(DEVICE_IDS.contains(&0xec20));
    }

    // ── MSI-X (interrupt-driven completions) ──

    fn up_msix() -> (MockBus, Ena<MockBus>) {
        let bus = MockBus::new(false);
        let mut ena = Ena::new(bus.clone(), false).unwrap();
        ena.set_msix_allowed(true);
        ena.init().expect("init");
        (bus, ena)
    }

    #[test]
    fn msix_off_by_default_keeps_polled_queues() {
        let (bus, ena) = up(false);
        let st = bus.st();
        assert!(!ena.msix_active());
        assert!(!st.tx.irq_mode && !st.rx.irq_mode, "CQs must be created polled");
        assert_eq!(st.intr_mask, INTR_MASK_ADMIN, "admin interrupt stays masked");
        assert_eq!(st.unmask_writes, 0);
    }

    #[test]
    fn msix_cqs_use_vector_one_and_admin_is_unmasked() {
        let (bus, ena) = up_msix();
        let st = bus.st();
        assert!(ena.msix_active());
        assert!(st.tx.irq_mode && st.rx.irq_mode, "both IO CQs are interrupt-mode");
        assert_eq!((st.tx.msix_vector, st.rx.msix_vector), (MSIX_IO0 as u32, MSIX_IO0 as u32));
        assert_eq!(MSIX_MGMT, 0);
        assert_eq!(st.intr_mask, 0, "vector 0 (AENQ) unmasked");
        assert!(st.violations.is_empty(), "{:?}", st.violations);
    }

    #[test]
    fn msix_unmask_doorbells_are_written_with_bit_30() {
        let (bus, _ena) = up_msix();
        let st = bus.st();
        let w: Vec<(usize, u32)> = st.reg_writes.iter().copied().filter(|(o, _)| *o == TX_CQ_UNMASK || *o == RX_CQ_UNMASK).collect();
        assert!(w.contains(&(TX_CQ_UNMASK, INTR_UNMASK)) && w.contains(&(RX_CQ_UNMASK, INTR_UNMASK)), "{:?}", w);
        assert!(!st.tx.irq_masked && !st.rx.irq_masked);
        assert_eq!(INTR_UNMASK, 1 << 30);
    }

    #[test]
    fn msix_completion_raises_one_interrupt_until_rearmed() {
        let (bus, mut ena) = up_msix();
        let mut buf = vec![0u8; 2048];
        let (rx0, tx0) = { let s = bus.st(); (s.rx.irqs, s.tx.irqs) };
        // A frame loops back: TX completion and RX frame each fire their CQ once.
        assert!(ena.send(&frame(1, 3)));
        { let s = bus.st(); assert_eq!(s.rx.irqs, rx0 + 1); assert_eq!(s.tx.irqs, tx0 + 1); assert!(s.rx.irq_masked); }
        // More traffic while masked: no further interrupts, frames wait in the ring.
        assert!(ena.send(&frame(2, 3)));
        assert!(ena.send(&frame(3, 3)));
        assert_eq!(bus.st().rx.irqs, rx0 + 1, "masked CQ must not fire again");
        assert!(ena.recv(&mut buf).is_some());
        assert!(ena.recv(&mut buf).is_some());
        // Work is still queued: re-arming reports it and the unmask itself fires the interrupt.
        assert!(ena.recv(&mut buf).is_some());
        let before = bus.st().unmask_writes;
        assert!(!ena.irq_rearm(), "drained ring: nothing pending");
        assert!(bus.st().unmask_writes > before, "rearm writes the unmask doorbell");
        // The mock re-signals completions queued while masked (spurious here: already drained);
        // a second re-arm finds nothing new and leaves the vector open.
        assert!(!ena.irq_rearm());
        assert!(!bus.st().rx.irq_masked);
        let irqs = bus.st().rx.irqs;
        // The next frame wakes us again.
        assert!(ena.send(&frame(4, 3)));
        assert_eq!(bus.st().rx.irqs, irqs + 1);
        assert!(ena.recv(&mut buf).is_some());
    }

    #[test]
    fn msix_rearm_with_pending_work_fires_immediately() {
        let (bus, mut ena) = up_msix();
        assert!(ena.send(&frame(1, 5)));          // fires, masks
        assert!(ena.send(&frame(2, 5)));          // pending behind the mask
        let irqs = bus.st().rx.irqs;
        assert!(ena.irq_rearm(), "frames are waiting");
        assert_eq!(bus.st().rx.irqs, irqs + 1, "unmasking with queued completions fires at once");
    }

    #[test]
    fn msix_loopback_10k_frames_never_loses_a_wakeup() {
        let (bus, mut ena) = up_msix();
        let mut buf = vec![0u8; 2048];
        let mut woke = 0u32;
        for n in 0..10_000usize {
            let f = frame(n, 11);
            let irqs = bus.st().rx.irqs;
            assert!(ena.send(&f), "send {}", n);
            if bus.st().rx.irqs != irqs { woke += 1; }
            let got = ena.recv(&mut buf).unwrap_or_else(|| panic!("no frame {}", n));
            assert_eq!(&buf[..f.len()], &f[..]);
            assert!(got >= f.len());
            assert!(!ena.irq_rearm());
        }
        assert_eq!(woke, 10_000, "every frame was announced by an interrupt after the re-arm");
        assert!(bus.st().violations.is_empty(), "{:?}", bus.st().violations);
    }

    #[test]
    fn msix_aenq_event_fires_vector_zero_only_when_unmasked() {
        let (bus, mut ena) = up_msix();
        bus.st().push_aenq(aenq::KEEP_ALIVE, 0);
        assert_eq!(bus.st().mgmt_irqs, 1);
        assert_eq!(ena.poll_aenq(), 1);
        let (pbus, _p) = up(false);
        pbus.st().push_aenq(aenq::KEEP_ALIVE, 0);
        assert_eq!(pbus.st().mgmt_irqs, 0, "polled: admin interrupt masked");
    }

    #[test]
    fn msix_refused_cq_falls_back_to_polled() {
        let bus = MockBus::new(false);
        bus.st().fail_irq_cq = true;
        let mut ena = Ena::new(bus.clone(), false).unwrap();
        ena.set_msix_allowed(true);
        ena.init().expect("init must retry polled");
        assert!(!ena.msix_active());
        { let s = bus.st(); assert!(!s.tx.irq_mode && !s.rx.irq_mode); assert_eq!(s.intr_mask, INTR_MASK_ADMIN); }
        let mut buf = vec![0u8; 2048];
        for n in 0..300 { let f = frame(n, 1); assert!(ena.send(&f)); assert!(ena.recv(&mut buf).is_some()); }
        assert!(!ena.irq_rearm());
        assert_eq!(bus.st().unmask_writes, 0);
    }

    #[test]
    fn msix_without_unmask_register_falls_back_to_polled() {
        let bus = MockBus::new(false);
        bus.st().no_unmask_reg = true;
        let mut ena = Ena::new(bus.clone(), false).unwrap();
        ena.set_msix_allowed(true);
        ena.init().expect("init must retry polled");
        assert!(!ena.msix_active());
        assert!(!bus.st().rx.irq_mode);
    }

    #[test]
    fn msix_survives_a_watchdog_reset() {
        let (bus, mut ena) = up_msix();
        bus.advance_ms(7000);
        assert_eq!(ena.watchdog(), Watchdog::Reset("keep-alive timeout"));
        assert!(ena.msix_active());
        { let s = bus.st(); assert!(s.rx.irq_mode && s.tx.irq_mode && !s.rx.irq_masked); assert_eq!(s.intr_mask, 0); }
        let irqs = bus.st().rx.irqs;
        let mut buf = vec![0u8; 2048];
        assert!(ena.send(&frame(1, 2)));
        assert_eq!(bus.st().rx.irqs, irqs + 1);
        assert!(ena.recv(&mut buf).is_some());
    }
}
