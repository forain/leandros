//! Simulated SDHCI 3.0 controller with an SDIO card behind it.
use crate::sdio_host::*;
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Op53 { pub write: bool, pub func: u8, pub addr: u32, pub block: bool, pub incr: bool, pub count: u32, pub arg: u32 }

pub struct MockHost {
    pub regs: [u8; 0x100],
    pub time: u64,
    /// Card register space: (func, addr) → byte.
    pub mem: HashMap<(u8, u32), u8>,
    pub ops53: Vec<Op53>,
    pub args52: Vec<u32>,
    pub cmds: Vec<u8>,
    pub cmd5_busy_polls: u32,
    pub cmd5_never_ready: bool,
    pub timeout_next: bool,
    pub card_irq: bool,
    /// R5 flag bits to put into the next CMD52 response.
    pub r5_flags: u8,
    pub swrst_writes: Vec<u8>,
    rx: VecDeque<u8>,
    tx: Vec<u8>,
    tx_expect: usize,
    cur: Option<Op53>,
    bsz: usize,
}

const CIS: u32 = 0x1000;

impl MockHost {
    pub fn new() -> Self {
        let mut m = MockHost { regs: [0; 0x100], time: 0, mem: HashMap::new(), ops53: vec![], args52: vec![],
            cmds: vec![], cmd5_busy_polls: 2, cmd5_never_ready: false, timeout_next: false, card_irq: false, r5_flags: 0,
            swrst_writes: vec![], rx: VecDeque::new(), tx: vec![], tx_expect: 0, cur: None, bsz: 0 };
        m.set16(REG_HOST_VERSION, 0x9902); // vendor 0x99, SDHCI 3.0
        m.set32(REG_CAPABILITIES, 200 << 8); // 200 MHz base
        m.set32(REG_PRESENT_STATE, 0x01F0_0000); // DAT lines high, idle
        // CCCR: SDIO 3.0 rev, caps SMB|4BLS, CIS at 0x1000, SHS.
        for (a, v) in [(0x00u32, 0x43u8), (0x08, 0x82), (0x09, 0x00), (0x0a, 0x10), (0x0b, 0x00), (0x13, 0x01)] {
            m.mem.insert((0, a), v);
        }
        // CIS: a vendor tuple to skip, then MANFID 0x02d0:0xa9bf, then end.
        let cis = [0x21u8, 0x02, 0x0c, 0x00, 0x20, 0x04, 0xd0, 0x02, 0xbf, 0xa9, 0xff];
        for (i, b) in cis.iter().enumerate() { m.mem.insert((0, CIS + i as u32), *b); }
        m
    }
    fn get16(&self, o: usize) -> u16 { u16::from_le_bytes([self.regs[o], self.regs[o + 1]]) }
    fn set16(&mut self, o: usize, v: u16) { self.regs[o..o + 2].copy_from_slice(&v.to_le_bytes()) }
    fn get32(&self, o: usize) -> u32 { u32::from_le_bytes(self.regs[o..o + 4].try_into().unwrap()) }
    fn set32(&mut self, o: usize, v: u32) { self.regs[o..o + 4].copy_from_slice(&v.to_le_bytes()) }
    fn raise(&mut self, bits: u16) { let v = self.get16(REG_NORMAL_INT_STATUS) | bits; self.set16(REG_NORMAL_INT_STATUS, v) }
    pub fn byte(&self, func: u8, addr: u32) -> u8 { *self.mem.get(&(func, addr)).unwrap_or(&0) }

    fn exec(&mut self, idx: u8) {
        let arg = self.get32(REG_ARGUMENT1);
        self.cmds.push(idx);
        if self.timeout_next {
            self.timeout_next = false;
            self.raise(NI_ERROR);
            self.set16(REG_ERROR_INT_STATUS, EI_CMD_TIMEOUT);
            return;
        }
        let resp = match idx {
            0 | 7 => 0,
            5 => {
                if arg == 0 { (2 << 28) | 0x00FF_8000 }
                else if self.cmd5_never_ready { 0x00FF_8000 }
                else if self.cmd5_busy_polls > 0 { self.cmd5_busy_polls -= 1; 0x00FF_8000 }
                else { 0x8000_0000 | (2 << 28) | 0x00FF_8000 }
            }
            3 => 0x0001_0000,
            52 => {
                self.args52.push(arg);
                let w = arg >> 31 != 0;
                let f = ((arg >> 28) & 7) as u8;
                let a = (arg >> 9) & 0x1FFFF;
                if w {
                    let mut d = arg as u8;
                    if f == 0 && a == 0x13 { d &= 0x03; d |= 0x01; } // SHS is read-only
                    if f == 1 && a == SBSDIO_FUNC1_CHIPCLKCSR && d & CLKCSR_ALP_AVAIL_REQ != 0 { d |= CLKCSR_ALP_AVAIL; }
                    self.mem.insert((f, a), d);
                    if f == 0 && a == 0x02 { self.mem.insert((0, 0x03), d); } // IORx follows IOEx
                }
                let fl = std::mem::take(&mut self.r5_flags) as u32;
                0x1000 | fl << 8 | self.byte(f, a) as u32
            }
            53 => {
                let op = Op53 { write: arg >> 31 != 0, func: ((arg >> 28) & 7) as u8, addr: (arg >> 9) & 0x1FFFF,
                    block: arg & (1 << 27) != 0, incr: arg & (1 << 26) != 0, count: arg & 0x1FF, arg };
                self.ops53.push(op);
                let bs = (self.get16(REG_BLOCK_SIZE) & 0xFFF) as usize;
                let bc = self.get16(REG_BLOCK_COUNT) as usize;
                let total = bs * bc;
                let expect = if op.block { op.count as usize * self.bsz_of(op.func) } else if op.count == 0 { 512 } else { op.count as usize };
                assert_eq!(total, expect, "host block regs disagree with CMD53 arg {:?}", op);
                self.bsz = bs;
                if op.write { self.tx.clear(); self.tx_expect = total; self.raise(NI_BUF_WRITE_READY); }
                else {
                    for i in 0..total as u32 {
                        let a = if op.incr { op.addr + i } else { op.addr };
                        let b = self.byte(op.func, a);
                        self.rx.push_back(b);
                    }
                    self.raise(NI_BUF_READ_READY);
                }
                self.cur = Some(op);
                0x1000
            }
            _ => 0,
        };
        self.set32(REG_RESPONSE0, resp);
        self.raise(NI_CMD_COMPLETE);
    }
    fn bsz_of(&self, func: u8) -> usize {
        let fbr = func as u32 * 0x100;
        self.byte(0, fbr + 0x10) as usize | (self.byte(0, fbr + 0x11) as usize) << 8
    }
}

impl HostIo for MockHost {
    fn r8(&mut self, o: usize) -> u8 { self.time += 1; self.regs[o] }
    fn r16(&mut self, o: usize) -> u16 {
        self.time += 1;
        let mut v = self.get16(o);
        if o == REG_NORMAL_INT_STATUS && self.card_irq && self.get16(REG_NORMAL_INT_ENABLE) & NI_CARD_INT != 0 { v |= NI_CARD_INT; }
        v
    }
    fn r32(&mut self, o: usize) -> u32 {
        self.time += 1;
        if o == REG_BUFFER_DATA {
            let mut w = [0u8; 4];
            for b in w.iter_mut() { *b = self.rx.pop_front().unwrap_or(0); }
            if self.rx.is_empty() {
                let v = self.get16(REG_NORMAL_INT_STATUS) & !NI_BUF_READ_READY;
                self.set16(REG_NORMAL_INT_STATUS, v);
                self.raise(NI_XFER_COMPLETE);
            } else if self.rx.len() % self.bsz == 0 {
                self.raise(NI_BUF_READ_READY); // next block buffered
            }
            return u32::from_le_bytes(w);
        }
        self.get32(o)
    }
    fn w8(&mut self, o: usize, v: u8) {
        if o == REG_SOFTWARE_RESET { self.swrst_writes.push(v); self.rx.clear(); return; } // self-clearing
        self.regs[o] = v;
    }
    fn w16(&mut self, o: usize, v: u16) {
        match o {
            REG_NORMAL_INT_STATUS => { let c = self.get16(o) & !(v & !NI_CARD_INT); self.set16(o, c); }
            REG_ERROR_INT_STATUS => { let c = self.get16(o) & !v; self.set16(o, c); if c == 0 { let n = self.get16(REG_NORMAL_INT_STATUS) & !NI_ERROR; self.set16(REG_NORMAL_INT_STATUS, n); } }
            REG_COMMAND => { self.set16(o, v); self.exec((v >> 8) as u8); }
            REG_CLOCK_CONTROL => { let s = if v & CLK_INTERNAL_EN != 0 { CLK_STABLE } else { 0 }; self.set16(o, v | s); }
            _ => self.set16(o, v),
        }
    }
    fn w32(&mut self, o: usize, v: u32) {
        if o == REG_BUFFER_DATA {
            self.tx.extend_from_slice(&v.to_le_bytes());
            if self.tx.len() >= self.tx_expect {
                let op = self.cur.unwrap();
                for i in 0..self.tx_expect {
                    let a = if op.incr { op.addr + i as u32 } else { op.addr };
                    self.mem.insert((op.func, a), self.tx[i]);
                }
                let s = self.get16(REG_NORMAL_INT_STATUS) & !NI_BUF_WRITE_READY;
                self.set16(REG_NORMAL_INT_STATUS, s);
                self.raise(NI_XFER_COMPLETE);
            } else if self.tx.len() % self.bsz == 0 {
                self.raise(NI_BUF_WRITE_READY);
            }
            return;
        }
        self.set32(o, v)
    }
    fn now_us(&mut self) -> u64 { self.time += 1; self.time }
    fn delay_us(&mut self, us: u32) { self.time += us as u64 }
}
