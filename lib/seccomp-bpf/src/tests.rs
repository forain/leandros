use super::*;

const LDA: u16 = BPF_LD | BPF_W | BPF_ABS;
const RETK: u16 = BPF_RET | BPF_K;
const JEQ: u16 = BPF_JMP | BPF_JEQ | BPF_K;
const AUDIT_X86_64: u32 = 0xc000_003e;
fn st(c: u16, k: u32) -> SockFilter { SockFilter::stmt(c, k) }
fn jm(c: u16, k: u32, t: u8, f: u8) -> SockFilter { SockFilter::jump(c, k, t, f) }
fn sd(nr: i32, arch: u32) -> SeccompData { SeccompData { nr, arch, ..Default::default() } }
fn prog(v: &[SockFilter]) -> Program { check_filter(v).unwrap() }

fn ff() -> Program {
    prog(&[
        st(LDA, 4),
        jm(JEQ, AUDIT_X86_64, 1, 0),
        st(RETK, SECCOMP_RET_KILL_PROCESS),
        st(LDA, 0),
        jm(JEQ, 1, 4, 0),
        jm(JEQ, 2, 3, 0),
        jm(JEQ, 3, 2, 0),
        jm(JEQ, 4, 1, 0),
        st(RETK, SECCOMP_RET_ALLOW),
        st(RETK, SECCOMP_RET_ERRNO | 1),
        st(RETK, SECCOMP_RET_TRAP),
        st(RETK, SECCOMP_RET_ALLOW),
    ])
}

#[test]
fn layout() {
    assert_eq!(core::mem::size_of::<SeccompData>(), 64);
    assert_eq!(core::mem::size_of::<SockFilter>(), 8);
    let d = SeccompData { nr: -1, arch: 7, instruction_pointer: 0x1122334455667788, args: [0, 1, 2, 3, 0xaabbccdd_eeff0011, 5] };
    let p = prog(&[st(LDA, 0), st(BPF_RET | BPF_A, 0)]);
    assert_eq!(run(&p, &d), u32::MAX);
    let w = |off| run(&prog(&[st(LDA, off), st(BPF_RET | BPF_A, 0)]), &d);
    assert_eq!(w(4), 7);
    assert_eq!(w(8), 0x55667788);
    assert_eq!(w(12), 0x11223344);
    assert_eq!(w(16 + 8 * 4), 0xeeff0011);
    assert_eq!(w(16 + 8 * 4 + 4), 0xaabbccdd);
    assert_eq!(w(60), 0);
}

#[test]
fn browser_style() {
    let p = ff();
    assert_eq!(run(&p, &sd(1, AUDIT_X86_64)), SECCOMP_RET_ERRNO | 1);
    assert_eq!(run(&p, &sd(5, AUDIT_X86_64)), SECCOMP_RET_ALLOW);
    assert_eq!(run(&p, &sd(1, 0x1234)), SECCOMP_RET_KILL_PROCESS);
}

#[test]
fn rejections() {
    let ret = st(RETK, 0);
    assert_eq!(check_filter(&[]).unwrap_err(), Error::BadLength);
    assert_eq!(check_filter(&alloc::vec![ret; 4097]).unwrap_err(), Error::BadLength);
    assert!(check_filter(&alloc::vec![ret; 4096]).is_ok());
    assert_eq!(check_filter(&[st(0xffff, 0), ret]).unwrap_err(), Error::BadOpcode(0));
    assert_eq!(check_filter(&[st(BPF_LD | BPF_B | BPF_ABS, 0), ret]).unwrap_err(), Error::BadOpcode(0));
    assert_eq!(check_filter(&[st(BPF_LD | BPF_H | BPF_ABS, 0), ret]).unwrap_err(), Error::BadOpcode(0));
    assert_eq!(check_filter(&[st(BPF_LD | BPF_W | BPF_IND, 0), ret]).unwrap_err(), Error::BadOpcode(0));
    assert_eq!(check_filter(&[st(BPF_LDX | BPF_B | BPF_MSH, 0), ret]).unwrap_err(), Error::BadOpcode(0));
    assert_eq!(check_filter(&[st(LDA, 2), ret]).unwrap_err(), Error::BadLoad(0));
    assert_eq!(check_filter(&[st(LDA, 64), ret]).unwrap_err(), Error::BadLoad(0));
    assert_eq!(check_filter(&[jm(JEQ, 0, 2, 0), ret]).unwrap_err(), Error::BadJump(0));
    assert_eq!(check_filter(&[jm(JEQ, 0, 0, 1), ret]).unwrap_err(), Error::BadJump(0));
    assert_eq!(check_filter(&[st(BPF_JMP | BPF_JA, 1), ret]).unwrap_err(), Error::BadJump(0));
    assert_eq!(check_filter(&[st(BPF_LD | BPF_MEM, 3), ret]).unwrap_err(), Error::UninitMem(0));
    assert_eq!(check_filter(&[st(BPF_ST, 16), ret]).unwrap_err(), Error::BadMem(0));
    assert_eq!(check_filter(&[st(LDA, 0)]).unwrap_err(), Error::NoRet);
    assert_eq!(check_filter(&[st(BPF_ALU | BPF_DIV | BPF_K, 0), ret]).unwrap_err(), Error::BadConstant(0));
    assert_eq!(check_filter(&[st(BPF_ALU | BPF_LSH | BPF_K, 32), ret]).unwrap_err(), Error::BadConstant(0));
    assert_eq!(check_filter(&[st(BPF_ALU | BPF_MOD | BPF_K, 3), ret]).unwrap_err(), Error::BadOpcode(0));
}

#[test]
fn mem_init_paths() {
    // store only on one branch -> read after join rejected
    let bad = [
        st(LDA, 0),
        jm(JEQ, 1, 1, 0),
        st(BPF_ST, 2),
        st(BPF_LD | BPF_MEM, 2),
        st(RETK, 0),
    ];
    assert_eq!(check_filter(&bad).unwrap_err(), Error::UninitMem(3));
    let good = [st(BPF_ST, 2), st(BPF_LD | BPF_MEM, 2), st(BPF_RET | BPF_A, 0)];
    assert_eq!(run(&prog(&good), &sd(0, 0)), 0);
}

#[test]
fn arithmetic() {
    let r = |v: &[SockFilter]| run(&prog(v), &sd(10, 0));
    let a = |c, k| st(BPF_ALU | c | BPF_K, k);
    let ra = st(BPF_RET | BPF_A, 0);
    assert_eq!(r(&[st(LDA, 0), a(BPF_ADD, u32::MAX), ra]), 9);
    assert_eq!(r(&[st(LDA, 0), a(BPF_SUB, 11), ra]), u32::MAX);
    assert_eq!(r(&[st(LDA, 0), a(BPF_MUL, 0x1000_0000), ra]), 0xa000_0000);
    assert_eq!(r(&[st(LDA, 0), a(BPF_DIV, 3), ra]), 3);
    assert_eq!(r(&[st(LDA, 0), a(BPF_LSH, 31), ra]), 0);
    assert_eq!(r(&[st(LDA, 0), st(BPF_ALU | BPF_NEG, 0), ra]), (-10i32) as u32);
    assert_eq!(r(&[st(LDA, 0), a(BPF_XOR, 3), a(BPF_AND, 0xf), a(BPF_OR, 0x10), ra]), 0x19);
    // div by X == 0 at runtime returns 0 from the program
    assert_eq!(r(&[st(LDA, 0), st(BPF_LDX | BPF_IMM, 0), st(BPF_ALU | BPF_DIV | BPF_X, 0), st(RETK, 5)]), 0);
    // shift by X masks to 5 bits
    assert_eq!(r(&[st(LDA, 0), st(BPF_LDX | BPF_IMM, 33), st(BPF_ALU | BPF_LSH | BPF_X, 0), ra]), 20);
    // TAX/TXA, LEN, jgt/jge/jset unsigned
    assert_eq!(r(&[st(BPF_LD | BPF_W | BPF_LEN, 0), ra]), 64);
    assert_eq!(r(&[st(BPF_LDX | BPF_W | BPF_LEN, 0), st(BPF_MISC | BPF_TXA, 0), ra]), 64);
    let j = |op, k| r(&[st(LDA, 0), jm(BPF_JMP | op | BPF_K, k, 0, 1), st(RETK, 1), st(RETK, 2)]);
    assert_eq!(j(BPF_JGT, 9), 1);
    assert_eq!(j(BPF_JGT, 10), 2);
    assert_eq!(j(BPF_JGE, 10), 1);
    assert_eq!(j(BPF_JSET, 2), 1);
    assert_eq!(j(BPF_JSET, 5), 2);
    assert_eq!(j(BPF_JGT, 0x8000_0000), 2); // unsigned
}

#[test]
fn stacking() {
    let c = |v| prog(&[st(RETK, v)]);
    let d = sd(0, 0);
    let allow = c(SECCOMP_RET_ALLOW);
    let kill = c(SECCOMP_RET_KILL_PROCESS);
    let e1 = c(SECCOMP_RET_ERRNO | 1);
    let e2 = c(SECCOMP_RET_ERRNO | 2);
    assert_eq!(run_stack([&allow, &kill], &d), SECCOMP_RET_KILL_PROCESS);
    assert_eq!(run_stack([&kill, &allow], &d), SECCOMP_RET_KILL_PROCESS);
    assert_eq!(run_stack([&allow, &e1], &d), SECCOMP_RET_ERRNO | 1);
    assert_eq!(run_stack([&allow, &c(SECCOMP_RET_KILL_THREAD)], &d), 0);
    assert_eq!(run_stack([&c(SECCOMP_RET_KILL_THREAD), &kill], &d), SECCOMP_RET_KILL_PROCESS);
    // equal actions: newest (first traversed) wins
    assert_eq!(run_stack([&e2, &e1], &d), SECCOMP_RET_ERRNO | 2);
    assert_eq!(run_stack([&e1, &e2], &d), SECCOMP_RET_ERRNO | 1);
    // order TRAP < ERRNO < TRACE < LOG < ALLOW
    assert_eq!(run_stack([&allow, &c(SECCOMP_RET_LOG), &c(SECCOMP_RET_TRACE), &e1, &c(SECCOMP_RET_TRAP)], &d), SECCOMP_RET_TRAP);
    assert_eq!(precedence_min(SECCOMP_RET_ALLOW, SECCOMP_RET_USER_NOTIF), SECCOMP_RET_USER_NOTIF);
    assert_eq!(run_stack([], &d), SECCOMP_RET_ALLOW);
}
