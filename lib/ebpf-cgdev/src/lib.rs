//! eBPF core for the cgroup v2 device controller (`BPF_PROG_TYPE_CGROUP_DEVICE`):
//! decoding, verifier and interpreter for the instruction subset crun, runc and
//! systemd generate. See the bpfdev design.
#![cfg_attr(not(test), no_std)]
extern crate alloc;
use alloc::boxed::Box;
use alloc::vec::Vec;

pub const BPF_MAXINSNS: usize = 4096;
pub const EINVAL: i32 = 22;
pub const EACCES: i32 = 13;

pub const BPF_DEVCG_ACC_MKNOD: u32 = 1;
pub const BPF_DEVCG_ACC_READ: u32 = 2;
pub const BPF_DEVCG_ACC_WRITE: u32 = 4;
pub const BPF_DEVCG_DEV_BLOCK: u32 = 1;
pub const BPF_DEVCG_DEV_CHAR: u32 = 2;

// Classes.
pub const BPF_LDX: u8 = 0x01;
pub const BPF_ALU: u8 = 0x04;
pub const BPF_JMP: u8 = 0x05;
pub const BPF_JMP32: u8 = 0x06;
pub const BPF_ALU64: u8 = 0x07;
// Modes / sizes / source.
pub const BPF_MEM: u8 = 0x60;
pub const BPF_W: u8 = 0x00;
pub const BPF_H: u8 = 0x08;
pub const BPF_B: u8 = 0x10;
pub const BPF_X: u8 = 0x08;
// ALU ops.
pub const BPF_ADD: u8 = 0x00;
pub const BPF_SUB: u8 = 0x10;
pub const BPF_MUL: u8 = 0x20;
pub const BPF_DIV: u8 = 0x30;
pub const BPF_OR: u8 = 0x40;
pub const BPF_AND: u8 = 0x50;
pub const BPF_LSH: u8 = 0x60;
pub const BPF_RSH: u8 = 0x70;
pub const BPF_NEG: u8 = 0x80;
pub const BPF_MOD: u8 = 0x90;
pub const BPF_XOR: u8 = 0xa0;
pub const BPF_MOV: u8 = 0xb0;
pub const BPF_ARSH: u8 = 0xc0;
// JMP ops.
pub const BPF_JA: u8 = 0x00;
pub const BPF_JEQ: u8 = 0x10;
pub const BPF_JGT: u8 = 0x20;
pub const BPF_JGE: u8 = 0x30;
pub const BPF_JSET: u8 = 0x40;
pub const BPF_JNE: u8 = 0x50;
pub const BPF_JSGT: u8 = 0x60;
pub const BPF_JSGE: u8 = 0x70;
pub const BPF_EXIT: u8 = 0x90;
pub const BPF_JLT: u8 = 0xa0;
pub const BPF_JLE: u8 = 0xb0;
pub const BPF_JSLT: u8 = 0xc0;
pub const BPF_JSLE: u8 = 0xd0;

/// `struct bpf_cgroup_dev_ctx` (12 bytes).
pub const CTX_SIZE: usize = 12;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DevCtx {
    /// `(access << 16) | dev_type`
    pub access_type: u32,
    pub major: u32,
    pub minor: u32,
}
impl DevCtx {
    pub const fn new(dev_type: u32, access: u32, major: u32, minor: u32) -> Self {
        DevCtx { access_type: (access << 16) | (dev_type & 0xffff), major, minor }
    }
    fn bytes(&self) -> [u8; CTX_SIZE] {
        let mut b = [0u8; CTX_SIZE];
        b[0..4].copy_from_slice(&self.access_type.to_le_bytes());
        b[4..8].copy_from_slice(&self.major.to_le_bytes());
        b[8..12].copy_from_slice(&self.minor.to_le_bytes());
        b
    }
}

/// One 8-byte eBPF instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Insn {
    pub code: u8,
    pub dst: u8,
    pub src: u8,
    pub off: i16,
    pub imm: i32,
}
impl Insn {
    pub const fn new(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Self {
        Insn { code, dst, src, off, imm }
    }
    pub fn decode(b: &[u8; 8]) -> Self {
        Insn {
            code: b[0],
            dst: b[1] & 0xf,
            src: b[1] >> 4,
            off: i16::from_le_bytes([b[2], b[3]]),
            imm: i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        }
    }
    pub fn encode(&self) -> [u8; 8] {
        let o = self.off.to_le_bytes();
        let i = self.imm.to_le_bytes();
        [self.code, (self.src << 4) | (self.dst & 0xf), o[0], o[1], i[0], i[1], i[2], i[3]]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Zero instructions, more than BPF_MAXINSNS, or byte length not a multiple of 8.
    BadLength,
    /// Opcode or reserved field not in the accepted subset.
    BadOpcode(usize),
    /// Register number out of range or r10 used.
    BadReg(usize),
    /// Backward jump or target outside the program.
    BadJump(usize),
    /// Shift by >= width, or division by zero / register divisor.
    BadConstant(usize),
    /// Read of a register that was never written.
    UninitReg(usize),
    /// Context read at a disallowed offset/width, or through a non-ctx pointer.
    BadCtxAccess(usize),
    /// Arithmetic on the context pointer.
    BadCtxArith(usize),
    /// Instruction no path reaches.
    Unreachable(usize),
    /// Execution can run past the last instruction.
    NoExit(usize),
    /// r0 is not the constant 0 or 1 at EXIT.
    BadReturn(usize),
}
impl Error {
    /// The errno `BPF_PROG_LOAD` returns for this failure.
    pub fn errno(&self) -> i32 {
        match self {
            Error::UninitReg(_) | Error::BadCtxAccess(_) | Error::BadCtxArith(_) => EACCES,
            _ => EINVAL,
        }
    }
    pub fn insn(&self) -> Option<usize> {
        match *self {
            Error::BadLength => None,
            Error::BadOpcode(i) | Error::BadReg(i) | Error::BadJump(i) | Error::BadConstant(i)
            | Error::UninitReg(i) | Error::BadCtxAccess(i) | Error::BadCtxArith(i)
            | Error::Unreachable(i) | Error::NoExit(i) | Error::BadReturn(i) => Some(i),
        }
    }
}

/// A verified program.
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
    /// Decode raw little-endian instruction bytes and verify them.
    pub fn load(bytes: &[u8]) -> Result<Program, Error> {
        if bytes.len() % 8 != 0 {
            return Err(Error::BadLength);
        }
        let v: Vec<Insn> = bytes
            .chunks_exact(8)
            .map(|c| Insn::decode(c.try_into().unwrap()))
            .collect();
        verify(&v)
    }
}

// ---------------------------------------------------------------- semantics

fn alu(op: u8, is64: bool, d: u64, s: u64) -> u64 {
    if is64 {
        match op {
            BPF_ADD => d.wrapping_add(s),
            BPF_SUB => d.wrapping_sub(s),
            BPF_MUL => d.wrapping_mul(s),
            BPF_DIV => if s == 0 { 0 } else { d / s },
            BPF_MOD => if s == 0 { d } else { d % s },
            BPF_OR => d | s,
            BPF_AND => d & s,
            BPF_XOR => d ^ s,
            BPF_LSH => d << (s & 63),
            BPF_RSH => d >> (s & 63),
            BPF_ARSH => ((d as i64) >> (s & 63)) as u64,
            BPF_NEG => (d as i64).wrapping_neg() as u64,
            _ => s, // MOV
        }
    } else {
        let (d, s) = (d as u32, s as u32);
        (match op {
            BPF_ADD => d.wrapping_add(s),
            BPF_SUB => d.wrapping_sub(s),
            BPF_MUL => d.wrapping_mul(s),
            BPF_DIV => if s == 0 { 0 } else { d / s },
            BPF_MOD => if s == 0 { d } else { d % s },
            BPF_OR => d | s,
            BPF_AND => d & s,
            BPF_XOR => d ^ s,
            BPF_LSH => d << (s & 31),
            BPF_RSH => d >> (s & 31),
            BPF_ARSH => ((d as i32) >> (s & 31)) as u32,
            BPF_NEG => (d as i32).wrapping_neg() as u32,
            _ => s,
        }) as u64
    }
}

fn cond(op: u8, is64: bool, a: u64, b: u64) -> bool {
    let (a, b, sa, sb) = if is64 {
        (a, b, a as i64, b as i64)
    } else {
        (a as u32 as u64, b as u32 as u64, a as u32 as i32 as i64, b as u32 as i32 as i64)
    };
    match op {
        BPF_JEQ => a == b,
        BPF_JNE => a != b,
        BPF_JGT => a > b,
        BPF_JGE => a >= b,
        BPF_JLT => a < b,
        BPF_JLE => a <= b,
        BPF_JSGT => sa > sb,
        BPF_JSGE => sa >= sb,
        BPF_JSLT => sa < sb,
        BPF_JSLE => sa <= sb,
        BPF_JSET => a & b != 0,
        _ => false,
    }
}

fn imm64(i: i32) -> u64 {
    i as i64 as u64
}

// ----------------------------------------------------------------- verifier

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reg {
    Uninit,
    Ctx,
    Scalar(Option<u64>),
}
type State = [Reg; 11];

fn merge(a: &mut State, b: &State) {
    for i in 0..11 {
        a[i] = match (a[i], b[i]) {
            (x, y) if x == y => x,
            (Reg::Scalar(_), Reg::Scalar(_)) => Reg::Scalar(None),
            _ => Reg::Uninit,
        };
    }
}

fn jmp_op_ok(op: u8) -> bool {
    matches!(
        op,
        BPF_JEQ | BPF_JNE | BPF_JGT | BPF_JGE | BPF_JLT | BPF_JLE | BPF_JSGT | BPF_JSGE
            | BPF_JSLT | BPF_JSLE | BPF_JSET
    )
}

/// Verify a cgroup-device program (forward-only control flow, so one pass in
/// index order over the DAG suffices).
pub fn verify(insns: &[Insn]) -> Result<Program, Error> {
    let n = insns.len();
    if n == 0 || n > BPF_MAXINSNS {
        return Err(Error::BadLength);
    }
    let mut states: Vec<Option<State>> = alloc::vec![None; n];
    let mut entry = [Reg::Uninit; 11];
    entry[1] = Reg::Ctx;
    states[0] = Some(entry);

    fn flow(states: &mut [Option<State>], to: usize, st: &State) {
        match &mut states[to] {
            Some(cur) => merge(cur, st),
            slot => *slot = Some(*st),
        }
    }

    for pc in 0..n {
        let Some(mut st) = states[pc] else {
            return Err(Error::Unreachable(pc));
        };
        let i = insns[pc];
        let class = i.code & 7;
        let op = i.code & 0xf0;
        let is_x = i.code & BPF_X != 0;
        if i.dst > 10 || i.src > 10 {
            return Err(Error::BadReg(pc));
        }
        match class {
            BPF_ALU | BPF_ALU64 => {
                let is64 = class == BPF_ALU64;
                if i.off != 0 {
                    return Err(Error::BadOpcode(pc));
                }
                if !matches!(
                    op,
                    BPF_ADD | BPF_SUB | BPF_MUL | BPF_DIV | BPF_OR | BPF_AND | BPF_LSH | BPF_RSH
                        | BPF_NEG | BPF_MOD | BPF_XOR | BPF_MOV | BPF_ARSH
                ) {
                    return Err(Error::BadOpcode(pc));
                }
                if i.dst == 10 || (is_x && i.src == 10) {
                    return Err(Error::BadReg(pc));
                }
                let d = i.dst as usize;
                if op == BPF_NEG {
                    if is_x || i.src != 0 || i.imm != 0 {
                        return Err(Error::BadOpcode(pc));
                    }
                } else if is_x {
                    if i.imm != 0 {
                        return Err(Error::BadOpcode(pc));
                    }
                    if matches!(op, BPF_DIV | BPF_MOD) {
                        return Err(Error::BadConstant(pc));
                    }
                } else if i.src != 0 {
                    return Err(Error::BadOpcode(pc));
                }
                if !is_x && op != BPF_NEG {
                    match op {
                        BPF_DIV | BPF_MOD if i.imm == 0 => return Err(Error::BadConstant(pc)),
                        BPF_LSH | BPF_RSH | BPF_ARSH => {
                            let w = if is64 { 64 } else { 32 };
                            if i.imm < 0 || i.imm >= w {
                                return Err(Error::BadConstant(pc));
                            }
                        }
                        _ => {}
                    }
                }
                // Source operand.
                let src = if op == BPF_NEG {
                    Reg::Scalar(Some(0))
                } else if is_x {
                    match st[i.src as usize] {
                        Reg::Uninit => return Err(Error::UninitReg(pc)),
                        Reg::Ctx if is64 && op == BPF_MOV => Reg::Ctx,
                        Reg::Ctx => return Err(Error::BadCtxArith(pc)),
                        r => r,
                    }
                } else {
                    Reg::Scalar(Some(imm64(i.imm)))
                };
                if op == BPF_MOV {
                    st[d] = match src {
                        Reg::Scalar(Some(v)) if !is64 => Reg::Scalar(Some(v as u32 as u64)),
                        Reg::Scalar(None) => Reg::Scalar(None),
                        r => r,
                    };
                } else {
                    let dv = match st[d] {
                        Reg::Uninit => return Err(Error::UninitReg(pc)),
                        Reg::Ctx => return Err(Error::BadCtxArith(pc)),
                        Reg::Scalar(v) => v,
                    };
                    let sv = match src {
                        Reg::Scalar(v) => v,
                        _ => return Err(Error::BadCtxArith(pc)),
                    };
                    st[d] = Reg::Scalar(match (dv, sv) {
                        (Some(a), Some(b)) => Some(alu(op, is64, a, b)),
                        _ => None,
                    });
                }
                if pc + 1 >= n {
                    return Err(Error::NoExit(pc));
                }
                flow(&mut states, pc + 1, &st);
            }
            BPF_LDX => {
                if i.code & 0xe0 != BPF_MEM || i.imm != 0 {
                    return Err(Error::BadOpcode(pc));
                }
                if i.dst == 10 || i.src == 10 {
                    return Err(Error::BadReg(pc));
                }
                let size = match i.code & 0x18 {
                    BPF_W => 4,
                    BPF_H => 2,
                    BPF_B => 1,
                    _ => return Err(Error::BadOpcode(pc)),
                };
                match st[i.src as usize] {
                    Reg::Uninit => return Err(Error::UninitReg(pc)),
                    Reg::Ctx => {}
                    Reg::Scalar(_) => return Err(Error::BadCtxAccess(pc)),
                }
                let off = i.off as i32;
                // Exact fields: access_type (0..4, narrow loads allowed), major (4, W), minor (8, W).
                let ok = off >= 0
                    && off % size == 0
                    && ((off + size <= 4) || (size == 4 && (off == 4 || off == 8)));
                if !ok {
                    return Err(Error::BadCtxAccess(pc));
                }
                st[i.dst as usize] = Reg::Scalar(None);
                if pc + 1 >= n {
                    return Err(Error::NoExit(pc));
                }
                flow(&mut states, pc + 1, &st);
            }
            BPF_JMP | BPF_JMP32 => {
                let is64 = class == BPF_JMP;
                if op == BPF_EXIT {
                    if !is64 || is_x || i.dst != 0 || i.src != 0 || i.off != 0 || i.imm != 0 {
                        return Err(Error::BadOpcode(pc));
                    }
                    match st[0] {
                        Reg::Scalar(Some(0)) | Reg::Scalar(Some(1)) => {}
                        Reg::Uninit => return Err(Error::UninitReg(pc)),
                        _ => return Err(Error::BadReturn(pc)),
                    }
                    continue;
                }
                if i.off < 0 {
                    return Err(Error::BadJump(pc));
                }
                let target = pc + 1 + i.off as usize;
                if target >= n {
                    return Err(Error::BadJump(pc));
                }
                if op == BPF_JA {
                    if !is64 || is_x || i.dst != 0 || i.src != 0 || i.imm != 0 {
                        return Err(Error::BadOpcode(pc));
                    }
                    flow(&mut states, target, &st);
                    continue;
                }
                if !jmp_op_ok(op) {
                    return Err(Error::BadOpcode(pc));
                }
                if i.dst == 10 || (is_x && i.src == 10) {
                    return Err(Error::BadReg(pc));
                }
                if is_x {
                    if i.imm != 0 {
                        return Err(Error::BadOpcode(pc));
                    }
                } else if i.src != 0 {
                    return Err(Error::BadOpcode(pc));
                }
                for r in [Some(i.dst), if is_x { Some(i.src) } else { None }].into_iter().flatten() {
                    match st[r as usize] {
                        Reg::Uninit => return Err(Error::UninitReg(pc)),
                        Reg::Ctx => return Err(Error::BadCtxArith(pc)),
                        _ => {}
                    }
                }
                if pc + 1 >= n {
                    return Err(Error::NoExit(pc));
                }
                flow(&mut states, pc + 1, &st);
                flow(&mut states, target, &st);
            }
            _ => return Err(Error::BadOpcode(pc)),
        }
    }
    Ok(Program { insns: insns.to_vec().into_boxed_slice() })
}

// -------------------------------------------------------------- interpreter

/// Run a verified program; returns r0 (0 = deny, 1 = allow).
pub fn run(prog: &Program, ctx: &DevCtx) -> u32 {
    let cb = ctx.bytes();
    let mut r = [0u64; 11];
    let mut pc = 0usize;
    // Forward-only, so at most len steps; the bound is belt and braces.
    for _ in 0..=prog.insns.len() {
        let i = prog.insns[pc];
        let class = i.code & 7;
        let op = i.code & 0xf0;
        let is_x = i.code & BPF_X != 0;
        pc += 1;
        match class {
            BPF_ALU | BPF_ALU64 => {
                let is64 = class == BPF_ALU64;
                let s = if is_x { r[i.src as usize] } else { imm64(i.imm) };
                r[i.dst as usize] = alu(op, is64, r[i.dst as usize], s);
            }
            BPF_LDX => {
                let o = i.off as usize;
                r[i.dst as usize] = match i.code & 0x18 {
                    BPF_W => u32::from_le_bytes(cb[o..o + 4].try_into().unwrap()) as u64,
                    BPF_H => u16::from_le_bytes(cb[o..o + 2].try_into().unwrap()) as u64,
                    _ => cb[o] as u64,
                };
            }
            _ => {
                if op == BPF_EXIT {
                    return r[0] as u32;
                }
                let taken = if op == BPF_JA {
                    true
                } else {
                    let s = if is_x { r[i.src as usize] } else { imm64(i.imm) };
                    cond(op, class == BPF_JMP, r[i.dst as usize], s)
                };
                if taken {
                    pc += i.off as usize;
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod tests;
