//! Seccomp classic-BPF core: verifier + interpreter, matching Linux
//! (`bpf_check_classic` + `seccomp_check_filter` + classic `__bpf_prog_run`).
#![cfg_attr(not(test), no_std)]
extern crate alloc;
use alloc::boxed::Box;
use alloc::vec::Vec;

pub const BPF_MAXINSNS: usize = 4096;
pub const BPF_MEMWORDS: usize = 16;

pub const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
pub const SECCOMP_RET_KILL_THREAD: u32 = 0;
pub const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
pub const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
pub const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
pub const SECCOMP_RET_TRACE: u32 = 0x7ff0_0000;
pub const SECCOMP_RET_LOG: u32 = 0x7ffc_0000;
pub const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
pub const SECCOMP_RET_ACTION_FULL: u32 = 0xffff_0000;
pub const SECCOMP_RET_ACTION: u32 = 0x7fff_0000;
pub const SECCOMP_RET_DATA: u32 = 0x0000_ffff;

// Opcode fields.
pub const BPF_LD: u16 = 0x00;
pub const BPF_LDX: u16 = 0x01;
pub const BPF_ST: u16 = 0x02;
pub const BPF_STX: u16 = 0x03;
pub const BPF_ALU: u16 = 0x04;
pub const BPF_JMP: u16 = 0x05;
pub const BPF_RET: u16 = 0x06;
pub const BPF_MISC: u16 = 0x07;
pub const BPF_W: u16 = 0x00;
pub const BPF_H: u16 = 0x08;
pub const BPF_B: u16 = 0x10;
pub const BPF_IMM: u16 = 0x00;
pub const BPF_ABS: u16 = 0x20;
pub const BPF_IND: u16 = 0x40;
pub const BPF_MEM: u16 = 0x60;
pub const BPF_LEN: u16 = 0x80;
pub const BPF_MSH: u16 = 0xa0;
pub const BPF_ADD: u16 = 0x00;
pub const BPF_SUB: u16 = 0x10;
pub const BPF_MUL: u16 = 0x20;
pub const BPF_DIV: u16 = 0x30;
pub const BPF_OR: u16 = 0x40;
pub const BPF_AND: u16 = 0x50;
pub const BPF_LSH: u16 = 0x60;
pub const BPF_RSH: u16 = 0x70;
pub const BPF_NEG: u16 = 0x80;
pub const BPF_MOD: u16 = 0x90;
pub const BPF_XOR: u16 = 0xa0;
pub const BPF_JA: u16 = 0x00;
pub const BPF_JEQ: u16 = 0x10;
pub const BPF_JGT: u16 = 0x20;
pub const BPF_JGE: u16 = 0x30;
pub const BPF_JSET: u16 = 0x40;
pub const BPF_K: u16 = 0x00;
pub const BPF_X: u16 = 0x08;
pub const BPF_A: u16 = 0x10;
pub const BPF_TAX: u16 = 0x00;
pub const BPF_TXA: u16 = 0x80;

/// Linux `struct seccomp_data` (64 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeccompData {
    pub nr: i32,
    pub arch: u32,
    pub instruction_pointer: u64,
    pub args: [u64; 6],
}
pub const SECCOMP_DATA_SIZE: u32 = 64;
const _: () = assert!(core::mem::size_of::<SeccompData>() == 64);

impl SeccompData {
    /// Load the 32-bit word at byte offset `off` (4-aligned, < 64), host endian.
    fn word(&self, off: u32) -> u32 {
        let i = (off / 4) as usize;
        match i {
            0 => self.nr as u32,
            1 => self.arch,
            2 => self.instruction_pointer as u32,
            3 => (self.instruction_pointer >> 32) as u32,
            _ => {
                let a = self.args[(i - 4) / 2];
                if i % 2 == 0 { a as u32 } else { (a >> 32) as u32 }
            }
        }
    }
}

/// Linux `struct sock_filter`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}
pub type Insn = SockFilter;

impl SockFilter {
    pub const fn new(code: u16, jt: u8, jf: u8, k: u32) -> Self {
        SockFilter { code, jt, jf, k }
    }
    pub const fn stmt(code: u16, k: u32) -> Self {
        Self::new(code, 0, 0, k)
    }
    pub const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Self {
        Self::new(code, jt, jf, k)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Program length 0 or > BPF_MAXINSNS.
    BadLength,
    /// Opcode not allowed (by seccomp whitelist).
    BadOpcode(usize),
    /// ABS load unaligned or out of seccomp_data.
    BadLoad(usize),
    /// Jump target beyond the program.
    BadJump(usize),
    /// Division / shift by an invalid constant.
    BadConstant(usize),
    /// Scratch-memory index >= 16.
    BadMem(usize),
    /// Read of M[k] not preceded by a store on every path.
    UninitMem(usize),
    /// Last instruction is not RET.
    NoRet,
}

/// A verified (and LEN-rewritten) program.
#[derive(Clone, Debug)]
pub struct Program {
    insns: Box<[Insn]>,
}
impl Program {
    pub fn insns(&self) -> &[Insn] {
        &self.insns
    }
    pub fn len(&self) -> usize {
        self.insns.len()
    }
    pub fn is_empty(&self) -> bool {
        self.insns.is_empty()
    }
}

/// Linux `bpf_check_classic` + `seccomp_check_filter` + `check_load_and_stores`.
pub fn check_filter(filter: &[SockFilter]) -> Result<Program, Error> {
    let flen = filter.len();
    if flen == 0 || flen > BPF_MAXINSNS {
        return Err(Error::BadLength);
    }
    let mut out: Vec<Insn> = Vec::with_capacity(flen);
    for (pc, f) in filter.iter().enumerate() {
        let mut f = *f;
        let c = f.code;
        let rem = (flen - pc - 1) as u32;
        // seccomp whitelist (with LEN rewrites)
        if c == BPF_LD | BPF_W | BPF_ABS {
            if f.k >= SECCOMP_DATA_SIZE || f.k & 3 != 0 {
                return Err(Error::BadLoad(pc));
            }
        } else if c == BPF_LD | BPF_W | BPF_LEN {
            f.code = BPF_LD | BPF_IMM;
            f.k = SECCOMP_DATA_SIZE;
        } else if c == BPF_LDX | BPF_W | BPF_LEN {
            f.code = BPF_LDX | BPF_IMM;
            f.k = SECCOMP_DATA_SIZE;
        } else if c == BPF_LD | BPF_IMM || c == BPF_LDX | BPF_IMM {
        } else if c == BPF_LD | BPF_MEM || c == BPF_LDX | BPF_MEM || c == BPF_ST || c == BPF_STX {
            if f.k as usize >= BPF_MEMWORDS {
                return Err(Error::BadMem(pc));
            }
        } else if c == BPF_MISC | BPF_TAX || c == BPF_MISC | BPF_TXA {
        } else if c == BPF_RET | BPF_K || c == BPF_RET | BPF_A {
        } else if c & 7 == BPF_ALU {
            let op = c & 0xf0;
            let x = c & BPF_X != 0;
            if c & !0xf8 != BPF_ALU {
                return Err(Error::BadOpcode(pc));
            }
            match op {
                BPF_ADD | BPF_SUB | BPF_MUL | BPF_AND | BPF_OR | BPF_XOR => {}
                BPF_DIV => {
                    if !x && f.k == 0 {
                        return Err(Error::BadConstant(pc));
                    }
                }
                BPF_LSH | BPF_RSH => {
                    if !x && f.k >= 32 {
                        return Err(Error::BadConstant(pc));
                    }
                }
                BPF_NEG => {
                    if x {
                        return Err(Error::BadOpcode(pc));
                    }
                }
                // BPF_MOD is not in Linux's seccomp whitelist.
                _ => return Err(Error::BadOpcode(pc)),
            }
        } else if c & 7 == BPF_JMP {
            if c & 0x7 != BPF_JMP || c & !0xf8 != BPF_JMP {
                return Err(Error::BadOpcode(pc));
            }
            match c & 0xf0 {
                BPF_JA => {
                    if c & BPF_X != 0 || f.k >= rem {
                        return Err(if c & BPF_X != 0 { Error::BadOpcode(pc) } else { Error::BadJump(pc) });
                    }
                }
                BPF_JEQ | BPF_JGT | BPF_JGE | BPF_JSET => {
                    if f.jt as u32 >= rem || f.jf as u32 >= rem {
                        return Err(Error::BadJump(pc));
                    }
                }
                _ => return Err(Error::BadOpcode(pc)),
            }
        } else {
            return Err(Error::BadOpcode(pc));
        }
        out.push(f);
    }
    let last = out[flen - 1].code;
    if last != BPF_RET | BPF_K && last != BPF_RET | BPF_A {
        return Err(Error::NoRet);
    }
    check_load_and_stores(&out)?;
    Ok(Program { insns: out.into_boxed_slice() })
}

/// Linux `check_load_and_stores`: every M[k] read must follow a store on all paths.
fn check_load_and_stores(p: &[Insn]) -> Result<(), Error> {
    let mut masks: Vec<u16> = alloc::vec![0xffff; p.len()];
    let mut valid: u16 = 0;
    for (pc, f) in p.iter().enumerate() {
        valid &= masks[pc];
        match f.code & 7 {
            x if x == BPF_ST || x == BPF_STX => valid |= 1 << f.k,
            x if x == BPF_LD || x == BPF_LDX => {
                if f.code & 0xe0 == BPF_MEM && valid & (1 << f.k) == 0 {
                    return Err(Error::UninitMem(pc));
                }
            }
            x if x == BPF_JMP => {
                if f.code & 0xf0 == BPF_JA {
                    let t = pc + 1 + f.k as usize;
                    masks[t] &= valid;
                } else {
                    masks[pc + 1 + f.jt as usize] &= valid;
                    masks[pc + 1 + f.jf as usize] &= valid;
                }
                valid = 0xffff;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Run a verified program. Division by X == 0 at runtime returns 0.
pub fn run(prog: &Program, d: &SeccompData) -> u32 {
    run_ticked(prog, d, &mut || {})
}

/// Instructions between `tick` calls inside one filter run.
pub const TICK_INSNS: u32 = 256;

/// Like `run`, but calls `tick` every `TICK_INSNS` executed instructions so
/// the caller can open an interrupt window inside one long filter.
pub fn run_ticked<T: FnMut()>(prog: &Program, d: &SeccompData, tick: &mut T) -> u32 {
    let p = &prog.insns;
    let mut budget = TICK_INSNS;
    let (mut a, mut x) = (0u32, 0u32);
    let mut m = [0u32; BPF_MEMWORDS];
    let mut pc = 0usize;
    loop {
        budget -= 1;
        if budget == 0 { budget = TICK_INSNS; tick(); }
        let f = p[pc];
        pc += 1;
        let c = f.code;
        let k = f.k;
        match c {
            _ if c == BPF_LD | BPF_W | BPF_ABS => a = d.word(k),
            _ if c == BPF_LD | BPF_IMM => a = k,
            _ if c == BPF_LDX | BPF_IMM => x = k,
            _ if c == BPF_LD | BPF_MEM => a = m[k as usize],
            _ if c == BPF_LDX | BPF_MEM => x = m[k as usize],
            _ if c == BPF_ST => m[k as usize] = a,
            _ if c == BPF_STX => m[k as usize] = x,
            _ if c == BPF_MISC | BPF_TAX => x = a,
            _ if c == BPF_MISC | BPF_TXA => a = x,
            _ if c == BPF_RET | BPF_K => return k,
            _ if c == BPF_RET | BPF_A => return a,
            _ if c & 7 == BPF_ALU => {
                let s = if c & BPF_X != 0 { x } else { k };
                match c & 0xf0 {
                    BPF_ADD => a = a.wrapping_add(s),
                    BPF_SUB => a = a.wrapping_sub(s),
                    BPF_MUL => a = a.wrapping_mul(s),
                    BPF_DIV => {
                        if s == 0 {
                            return 0;
                        }
                        a /= s
                    }
                    BPF_AND => a &= s,
                    BPF_OR => a |= s,
                    BPF_XOR => a ^= s,
                    BPF_LSH => a <<= s & 31,
                    BPF_RSH => a >>= s & 31,
                    _ => a = a.wrapping_neg(),
                }
            }
            _ => {
                // JMP
                let s = if c & BPF_X != 0 { x } else { k };
                let op = c & 0xf0;
                if op == BPF_JA {
                    pc += k as usize;
                } else {
                    let t = match op {
                        BPF_JEQ => a == s,
                        BPF_JGT => a > s,
                        BPF_JGE => a >= s,
                        _ => a & s != 0,
                    };
                    pc += if t { f.jt } else { f.jf } as usize;
                }
            }
        }
    }
}

/// Action part used for precedence: `(s32)ret & SECCOMP_RET_ACTION_FULL` as signed,
/// so KILL_PROCESS (0x80000000) is the smallest, i.e. highest precedence.
pub fn action_only(ret: u32) -> i32 {
    (ret & SECCOMP_RET_ACTION_FULL) as i32
}

/// One step of Linux `seccomp_run_filters`: `cur` replaces `best` only if strictly
/// more restrictive. Fold from the NEWEST filter to the oldest starting at
/// `SECCOMP_RET_ALLOW`; on equal actions the earlier-seen (newest) one stays.
pub fn precedence_min(best: u32, cur: u32) -> u32 {
    if action_only(cur) < action_only(best) { cur } else { best }
}

/// Run stacked filters (`newest_first` order, as Linux walks `f->prev`).
pub fn run_stack<'a, I: IntoIterator<Item = &'a Program>>(newest_first: I, d: &SeccompData) -> u32 {
    let mut ret = SECCOMP_RET_ALLOW;
    for p in newest_first {
        ret = precedence_min(ret, run(p, d));
    }
    ret
}

/// `run_stack` with `tick` called between filters and every `TICK_INSNS`
/// instructions inside each filter.
pub fn run_stack_ticked<'a, I: IntoIterator<Item = &'a Program>, T: FnMut()>(
    newest_first: I, d: &SeccompData, tick: &mut T) -> u32 {
    let mut ret = SECCOMP_RET_ALLOW;
    for p in newest_first {
        tick();
        ret = precedence_min(ret, run_ticked(p, d, tick));
    }
    ret
}

#[cfg(test)]
mod tests;
