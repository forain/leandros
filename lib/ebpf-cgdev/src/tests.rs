use super::*;

const C: u32 = BPF_DEVCG_DEV_CHAR;
const B: u32 = BPF_DEVCG_DEV_BLOCK;
const M: u32 = BPF_DEVCG_ACC_MKNOD;
const R: u32 = BPF_DEVCG_ACC_READ;
const W: u32 = BPF_DEVCG_ACC_WRITE;
const ALL: u32 = 7;

fn i(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Insn {
    Insn::new(code, dst, src, off, imm)
}
fn ldx(dst: u8, src: u8, off: i16) -> Insn { i(BPF_LDX | BPF_MEM | BPF_W, dst, src, off, 0) }
fn mov_k(dst: u8, imm: i32) -> Insn { i(BPF_ALU64 | BPF_MOV, dst, 0, 0, imm) }
fn exit() -> Insn { i(BPF_JMP | BPF_EXIT, 0, 0, 0, 0) }
fn jne_k(dst: u8, imm: i32, off: i16) -> Insn { i(BPF_JMP | BPF_JNE, dst, 0, off, imm) }

/// One OCI device rule as crun sees it (type 'a'/'c'/'b', -1 wildcards).
#[derive(Clone, Copy)]
struct Rule { ty: u8, major: i32, minor: i32, acc: u32, allow: bool }
const fn rule(ty: u8, major: i32, minor: i32, acc: u32, allow: bool) -> Rule {
    Rule { ty, major, minor, acc, allow }
}
impl Rule {
    fn wildcard(&self) -> bool { self.ty == b'a' && self.major < 0 && self.minor < 0 && self.acc == ALL }
}

/// Port of crun's `create_dev_bpf` generator (ebpf.c): preamble, rules in reverse
/// OCI order (later rules win), generation stops at a full wildcard, otherwise a
/// default-deny tail.
fn crun_insns(rules: &[Rule]) -> Vec<Insn> {
    let mut v = alloc::vec![
        ldx(2, 1, 0),
        i(BPF_ALU | BPF_AND, 2, 0, 0, 0xffff),
        ldx(3, 1, 0),
        i(BPF_ALU | BPF_RSH, 3, 0, 0, 16),
        ldx(4, 1, 4),
        ldx(5, 1, 8),
    ];
    let mut wildcard = false;
    for r in rules.iter().rev() {
        let mut rv: Vec<Insn> = Vec::new();
        let mut jumps: Vec<usize> = Vec::new();
        if r.ty != b'a' {
            let t = if r.ty == b'b' { B } else { C };
            jumps.push(rv.len());
            rv.push(jne_k(2, t as i32, 0));
        }
        if r.acc != ALL {
            rv.push(i(BPF_ALU | BPF_MOV | BPF_X, 1, 3, 0, 0));
            rv.push(i(BPF_ALU | BPF_AND, 1, 0, 0, r.acc as i32));
            jumps.push(rv.len());
            rv.push(i(BPF_JMP | BPF_JNE | BPF_X, 1, 3, 0, 0));
        }
        if r.major >= 0 {
            jumps.push(rv.len());
            rv.push(jne_k(4, r.major, 0));
        }
        if r.minor >= 0 {
            jumps.push(rv.len());
            rv.push(jne_k(5, r.minor, 0));
        }
        rv.push(mov_k(0, r.allow as i32));
        rv.push(exit());
        let n = rv.len();
        for j in jumps {
            rv[j].off = (n - j - 1) as i16;
        }
        v.extend(rv);
        if r.wildcard() {
            wildcard = true;
            break;
        }
    }
    if !wildcard {
        v.push(mov_k(0, 0));
        v.push(exit());
    }
    v
}

fn to_bytes(v: &[Insn]) -> Vec<u8> { v.iter().flat_map(|x| x.encode()).collect() }
fn load(rules: &[Rule]) -> Program { Program::load(&to_bytes(&crun_insns(rules))).unwrap() }
fn ok(p: &Program, ty: u32, acc: u32, maj: u32, min: u32) -> bool {
    run(p, &DevCtx::new(ty, acc, maj, min)) == 1
}

/// Independent reference: last matching rule wins, default deny.
fn model(rules: &[Rule], ty: u32, acc: u32, maj: u32, min: u32) -> bool {
    for r in rules.iter().rev() {
        let t = match r.ty { b'a' => true, b'c' => ty == C, _ => ty == B };
        let a = acc & r.acc == acc;
        let ma = r.major < 0 || r.major as u32 == maj;
        let mi = r.minor < 0 || r.minor as u32 == min;
        if t && a && ma && mi {
            return r.allow;
        }
    }
    false
}

fn default_rules() -> Vec<Rule> {
    alloc::vec![
        rule(b'a', -1, -1, ALL, false),
        rule(b'c', 1, 3, ALL, true),   // null
        rule(b'c', 1, 5, ALL, true),   // zero
        rule(b'c', 1, 7, ALL, true),   // full
        rule(b'c', 5, 0, ALL, true),   // tty
        rule(b'c', 1, 8, ALL, true),   // random
        rule(b'c', 1, 9, ALL, true),   // urandom
        rule(b'c', 5, 1, ALL, true),   // console
        rule(b'c', 5, 2, ALL, true),   // ptmx
        rule(b'c', 136, -1, ALL, true), // pts
        rule(b'c', 10, 200, ALL, true), // tun
        rule(b'c', -1, -1, M, true),
        rule(b'b', -1, -1, M, true),
    ]
}

#[test]
fn insn_roundtrip() {
    let x = i(BPF_JMP | BPF_JNE, 4, 0, 7, -5);
    assert_eq!(Insn::decode(&x.encode()), x);
    let y = i(BPF_ALU | BPF_MOV | BPF_X, 1, 3, 0, 0);
    assert_eq!(y.encode(), [0xbc, 0x31, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn default_oci_matrix() {
    let rules = default_rules();
    let p = load(&rules);
    // Allowed devices, rwm.
    for (maj, min) in [(1, 3), (1, 5), (1, 7), (5, 0), (1, 8), (1, 9), (5, 1), (5, 2), (136, 0), (136, 77), (10, 200)] {
        assert!(ok(&p, C, R | W, maj, min), "c {maj}:{min} rw");
        assert!(ok(&p, C, ALL, maj, min), "c {maj}:{min} rwm");
    }
    // Denied.
    assert!(!ok(&p, C, R, 29, 0)); // fb0
    assert!(!ok(&p, C, R | W, 10, 201));
    assert!(!ok(&p, C, R, 1, 4));
    assert!(!ok(&p, B, R, 259, 0)); // nvme
    assert!(!ok(&p, B, W, 8, 0));
    assert!(!ok(&p, B, R | W, 7, 0)); // loop
    assert!(!ok(&p, B, R, 1, 3)); // type mismatch with null
    assert!(!ok(&p, C, R, 136 + 1, 0));
    // mknod anywhere, but not open.
    assert!(ok(&p, C, M, 29, 0));
    assert!(ok(&p, B, M, 259, 0));
    assert!(ok(&p, C, M, 1, 3));
    assert!(!ok(&p, C, M | R, 29, 0));
    // The model agrees on a dense grid.
    for ty in [C, B] {
        for acc in 1..=7u32 {
            for maj in [0, 1, 5, 10, 29, 136, 259] {
                for min in [0, 1, 2, 3, 5, 7, 8, 9, 77, 200, 201] {
                    assert_eq!(ok(&p, ty, acc, maj, min), model(&rules, ty, acc, maj, min), "{ty} {acc} {maj}:{min}");
                }
            }
        }
    }
}

#[test]
fn privileged_wildcard() {
    let rules = [rule(b'a', -1, -1, ALL, false), rule(b'a', -1, -1, ALL, true)];
    let insns = crun_insns(&rules);
    assert_eq!(insns.len(), 6 + 2);
    let p = Program::load(&to_bytes(&insns)).unwrap();
    for (ty, acc, maj, min) in [(C, ALL, 29, 0), (B, R, 259, 0), (C, M, 0, 0), (B, W, 8, 0)] {
        assert!(ok(&p, ty, acc, maj, min));
    }
}

#[test]
fn deny_all_only() {
    let p = load(&[rule(b'a', -1, -1, ALL, false)]);
    assert!(!ok(&p, C, R, 1, 3));
    assert!(!ok(&p, B, M, 8, 0));
}

#[test]
fn no_rules_default_deny_tail() {
    let p = load(&[]);
    assert!(!ok(&p, C, R, 1, 3));
}

#[test]
fn device_cgroup_rule_c10_rw() {
    let mut rules = default_rules();
    rules.push(rule(b'c', 10, -1, R | W, true));
    let p = load(&rules);
    assert!(ok(&p, C, R, 10, 55));
    assert!(ok(&p, C, R | W, 10, 0));
    assert_eq!(ok(&p, C, M, 10, 55), model(&rules, C, M, 10, 55));
    assert!(!ok(&p, C, ALL, 10, 55)); // m not covered by rw, and no earlier full rule
    assert!(!ok(&p, B, R, 10, 55));
    assert!(!ok(&p, C, R, 11, 55));
}

#[test]
fn later_rule_overrides_earlier() {
    let rules = [
        rule(b'a', -1, -1, ALL, false),
        rule(b'c', 1, -1, ALL, true),
        rule(b'c', 1, 5, ALL, false), // deny zero again
    ];
    let p = load(&rules);
    assert!(ok(&p, C, R, 1, 3));
    assert!(!ok(&p, C, R, 1, 5));
    assert!(!ok(&p, B, R, 1, 3));
}

#[test]
fn read_only_block_rule() {
    let rules = [rule(b'a', -1, -1, ALL, false), rule(b'b', 8, 0, R, true)];
    let p = load(&rules);
    assert!(ok(&p, B, R, 8, 0));
    assert!(!ok(&p, B, R | W, 8, 0));
    assert!(!ok(&p, B, W, 8, 0));
    assert!(!ok(&p, C, R, 8, 0));
    assert!(!ok(&p, B, R, 8, 1));
}

#[test]
fn runc_style_program() {
    // ALU32 MOV r0, JEQ, JA, narrow loads, JMP32, join with equal consts.
    let v = [
        i(BPF_LDX | BPF_MEM | BPF_B, 2, 1, 0, 0),
        i(BPF_LDX | BPF_MEM | BPF_H, 3, 1, 2, 0),
        ldx(4, 1, 4),
        i(BPF_JMP | BPF_JEQ, 2, 0, 3, C as i32),       // -> 7
        i(BPF_ALU | BPF_MOV, 0, 0, 0, 0),
        i(BPF_JMP | BPF_JA, 0, 0, 0, 0),
        exit(),
        i(BPF_JMP32 | BPF_JNE, 4, 0, 3, 1),            // 7: -> 11
        i(BPF_ALU | BPF_MOV, 0, 0, 0, 1),
        i(BPF_JMP | BPF_JA, 0, 0, 0, 0),
        exit(),
        i(BPF_ALU | BPF_MOV, 0, 0, 0, 0),            
        exit(),
    ];
    let p = Program::load(&to_bytes(&v)).unwrap();
    assert!(ok(&p, C, R, 1, 0));
    assert!(!ok(&p, C, R, 2, 0));
    assert!(!ok(&p, B, R, 1, 0));
}

#[test]
fn errno_mapping() {
    assert_eq!(Error::BadLength.errno(), EINVAL);
    assert_eq!(Error::UninitReg(0).errno(), EACCES);
    assert_eq!(Error::BadCtxAccess(0).errno(), EACCES);
    assert_eq!(Error::BadReturn(0).errno(), EINVAL);
    assert_eq!(Error::BadJump(0).errno(), EINVAL);
}

// ------------------------------------------------------------- negative

fn bad(v: &[Insn]) -> Error { verify(v).unwrap_err() }

#[test]
fn neg_backward_jump() {
    let e = bad(&[mov_k(0, 1), i(BPF_JMP | BPF_JA, 0, 0, -2, 0), exit()]);
    assert_eq!(e, Error::BadJump(1));
    assert_eq!(e.errno(), EINVAL);
    // Self loop.
    assert_eq!(bad(&[mov_k(0, 1), i(BPF_JMP | BPF_JA, 0, 0, -1, 0), exit()]), Error::BadJump(1));
    // Conditional backward.
    assert_eq!(
        bad(&[mov_k(0, 1), mov_k(2, 0), jne_k(2, 1, -3), exit()]),
        Error::BadJump(2)
    );
}

#[test]
fn neg_jump_out_of_range() {
    assert_eq!(bad(&[mov_k(0, 1), i(BPF_JMP | BPF_JA, 0, 0, 5, 0), exit()]), Error::BadJump(1));
}

#[test]
fn neg_ctx_reads() {
    let mk = |code: u8, off: i16| [i(code, 2, 1, off, 0), mov_k(0, 1), exit()];
    let w = BPF_LDX | BPF_MEM | BPF_W;
    assert!(verify(&mk(w, 0)).is_ok());
    assert!(verify(&mk(w, 4)).is_ok());
    assert!(verify(&mk(w, 8)).is_ok());
    for off in [12i16, 16, 9, 2, 6, -4, 100] {
        let e = bad(&mk(w, off));
        assert_eq!(e, Error::BadCtxAccess(0), "off {off}");
        assert_eq!(e.errno(), EACCES);
    }
    // Narrow loads only inside access_type.
    assert!(verify(&mk(BPF_LDX | BPF_MEM | BPF_H, 2)).is_ok());
    assert!(verify(&mk(BPF_LDX | BPF_MEM | BPF_B, 3)).is_ok());
    assert_eq!(bad(&mk(BPF_LDX | BPF_MEM | BPF_H, 4)), Error::BadCtxAccess(0));
    assert_eq!(bad(&mk(BPF_LDX | BPF_MEM | BPF_B, 8)), Error::BadCtxAccess(0));
    assert_eq!(bad(&mk(BPF_LDX | BPF_MEM | BPF_H, 1)), Error::BadCtxAccess(0));
    // 64-bit load is not a ctx field.
    assert_eq!(bad(&mk(BPF_LDX | BPF_MEM | 0x18, 0)), Error::BadOpcode(0));
    // Load through a scalar.
    assert_eq!(
        bad(&[mov_k(2, 0), i(BPF_LDX | BPF_MEM | BPF_W, 3, 2, 0, 0), mov_k(0, 1), exit()]),
        Error::BadCtxAccess(1)
    );
}

#[test]
fn neg_uninit_register() {
    // r2 never written.
    let e = bad(&[i(BPF_ALU64 | BPF_MOV | BPF_X, 0, 2, 0, 0), exit()]);
    assert_eq!(e, Error::UninitReg(0));
    assert_eq!(e.errno(), EACCES);
    // ALU on an uninit dst.
    assert_eq!(bad(&[i(BPF_ALU | BPF_AND, 2, 0, 0, 1), mov_k(0, 1), exit()]), Error::UninitReg(0));
    // Compare of an uninit reg.
    assert_eq!(bad(&[jne_k(3, 1, 0), mov_k(0, 1), exit()]), Error::UninitReg(0));
    // Written on only one path.
    let v = [
        ldx(2, 1, 0),
        jne_k(2, 1, 1),
        mov_k(3, 7),
        i(BPF_ALU64 | BPF_MOV | BPF_X, 0, 3, 0, 0),
        exit(),
    ];
    assert_eq!(bad(&v), Error::UninitReg(3));
    // r0 never set.
    assert_eq!(bad(&[exit()]), Error::UninitReg(0));
}

#[test]
fn neg_missing_exit() {
    assert_eq!(bad(&[mov_k(0, 1)]), Error::NoExit(0));
    assert_eq!(bad(&[mov_k(0, 1), jne_k(0, 5, 0)]), Error::BadJump(1));
    // JA as last insn jumps forward past the end.
    assert_eq!(bad(&[mov_k(0, 1), i(BPF_JMP | BPF_JA, 0, 0, 0, 0)]), Error::BadJump(1));
}

#[test]
fn neg_return_value() {
    for v in [2, -1, 255] {
        let e = bad(&[mov_k(0, v), exit()]);
        assert_eq!(e, Error::BadReturn(1));
        assert_eq!(e.errno(), EINVAL);
    }
    // Non-constant r0.
    assert_eq!(bad(&[ldx(0, 1, 4), exit()]), Error::BadReturn(1));
    // Constants that differ on joined paths.
    let v = [
        ldx(2, 1, 0),
        mov_k(0, 0),
        jne_k(2, 1, 1),
        mov_k(0, 1),
        i(BPF_ALU64 | BPF_ADD, 0, 0, 0, 1), // 1 or 2 -> not const
        exit(),
    ];
    assert_eq!(bad(&v), Error::BadReturn(5));
    // Computed constant is fine: 3 & 1 = 1.
    assert!(verify(&[mov_k(0, 3), i(BPF_ALU64 | BPF_AND, 0, 0, 0, 1), exit()]).is_ok());
}

#[test]
fn neg_size() {
    assert_eq!(verify(&[]).unwrap_err(), Error::BadLength);
    assert_eq!(Program::load(&[]).unwrap_err(), Error::BadLength);
    assert_eq!(Program::load(&[0u8; 9]).unwrap_err(), Error::BadLength);
    let mut v = alloc::vec![mov_k(0, 1); BPF_MAXINSNS];
    *v.last_mut().unwrap() = exit();
    assert!(verify(&v).is_ok());
    v.insert(0, mov_k(0, 1));
    let e = bad(&v);
    assert_eq!(e, Error::BadLength);
    assert_eq!(e.errno(), EINVAL);
}

#[test]
fn neg_unsupported_ops() {
    let tail = [mov_k(0, 1), exit()];
    let with = |x: Insn| { let mut v = alloc::vec![x]; v.extend_from_slice(&tail); v };
    // CALL
    assert_eq!(bad(&with(i(BPF_JMP | 0x80, 0, 0, 0, 1))), Error::BadOpcode(0));
    // LD_IMM64
    assert_eq!(bad(&with(i(0x18, 2, 0, 0, 1))), Error::BadOpcode(0));
    // ST / STX
    assert_eq!(bad(&with(i(0x62, 1, 0, 0, 1))), Error::BadOpcode(0));
    assert_eq!(bad(&with(i(0x63, 1, 2, 0, 0))), Error::BadOpcode(0));
    // LD ABS
    assert_eq!(bad(&with(i(0x20, 0, 0, 0, 0))), Error::BadOpcode(0));
    // r10 use.
    assert_eq!(bad(&with(i(BPF_ALU64 | BPF_MOV | BPF_X, 2, 10, 0, 0))), Error::BadReg(0));
    assert_eq!(bad(&with(mov_k(10, 0))), Error::BadReg(0));
    assert_eq!(bad(&with(ldx(2, 10, 0))), Error::BadReg(0));
    // Register out of range.
    assert_eq!(bad(&with(i(BPF_ALU64 | BPF_MOV, 11, 0, 0, 0))), Error::BadReg(0));
    // Shift width, div by zero, register divisor, byteswap.
    assert_eq!(bad(&with(i(BPF_ALU | BPF_LSH, 1, 0, 0, 32))), Error::BadConstant(0));
    assert_eq!(bad(&with(i(BPF_ALU64 | BPF_RSH, 1, 0, 0, 64))), Error::BadConstant(0));
    assert_eq!(bad(&with(i(BPF_ALU64 | BPF_DIV, 1, 0, 0, 0))), Error::BadConstant(0));
    assert_eq!(bad(&with(i(BPF_ALU64 | BPF_MOD | BPF_X, 1, 1, 0, 0))), Error::BadConstant(0));
    assert_eq!(bad(&with(i(BPF_ALU | 0xd0, 1, 0, 0, 16))), Error::BadOpcode(0));
}

#[test]
fn neg_ctx_pointer_misuse() {
    let tail = [mov_k(0, 1), exit()];
    let with = |x: Insn| { let mut v = alloc::vec![x]; v.extend_from_slice(&tail); v };
    let e = bad(&with(i(BPF_ALU64 | BPF_ADD, 1, 0, 0, 4)));
    assert_eq!(e, Error::BadCtxArith(0));
    assert_eq!(e.errno(), EACCES);
    assert_eq!(bad(&with(jne_k(1, 0, 0))), Error::BadCtxArith(0));
}

#[test]
fn neg_unreachable() {
    assert_eq!(
        bad(&[mov_k(0, 1), exit(), mov_k(0, 0), exit()]),
        Error::Unreachable(2)
    );
}

#[test]
fn neg_reserved_fields() {
    let tail = [mov_k(0, 1), exit()];
    let with = |x: Insn| { let mut v = alloc::vec![x]; v.extend_from_slice(&tail); v };
    assert_eq!(bad(&with(i(BPF_ALU64 | BPF_MOV, 2, 3, 0, 0))), Error::BadOpcode(0));
    assert_eq!(bad(&with(i(BPF_ALU64 | BPF_MOV, 2, 0, 1, 0))), Error::BadOpcode(0));
    assert_eq!(bad(&[mov_k(0, 1), i(BPF_JMP | BPF_EXIT, 0, 0, 0, 1)]), Error::BadOpcode(1));
}

#[test]
fn interpreter_arith() {
    // ALU32 zero-extends; imm sign-extends in 64-bit compares.
    let p = verify(&[
        mov_k(2, -1),                                 // r2 = 0xffff..ff
        i(BPF_ALU | BPF_ADD, 2, 0, 0, 0),             // r2 = 0x0000_0000_ffff_ffff
        i(BPF_JMP | BPF_JEQ, 2, 0, 2, -1),            // 64-bit: not equal to sign-ext -1
        mov_k(0, 1),
        exit(),
        mov_k(0, 0),
        exit(),
    ]);
    // JEQ off 2 targets idx 5; fall-through writes r0=1 and exits.
    let p = p.unwrap();
    assert_eq!(run(&p, &DevCtx::default()), 1);
}
