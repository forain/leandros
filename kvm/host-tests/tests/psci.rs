use core::cell::RefCell;
use kvm::psci::*;

struct Vm {
    st: RefCell<[CpuState; 4]>,
    started: RefCell<Vec<(u32, u64, u64)>>,
}
impl Vm {
    fn new() -> Self {
        Vm {
            st: RefCell::new([CpuState::On, CpuState::Off, CpuState::OnPending, CpuState::Off]),
            started: RefCell::new(vec![]),
        }
    }
}
impl PsciVm for Vm {
    fn nr_vcpus(&self) -> u32 {
        4
    }
    fn vcpu_mpidr(&self, v: u32) -> u64 {
        0x8000_0000 | v as u64
    }
    fn vcpu_state(&self, v: u32) -> CpuState {
        self.st.borrow()[v as usize]
    }
    fn cpu_on(&self, t: u32, e: u64, c: u64) -> Result<(), i64> {
        self.st.borrow_mut()[t as usize] = CpuState::OnPending;
        self.started.borrow_mut().push((t, e, c));
        Ok(())
    }
}

fn call(vm: &Vm, a: [u64; 4]) -> (u64, PsciOut) {
    let mut x = a;
    let o = handle(vm, 0, &mut x);
    (x[0], o)
}
const E: PsciOut = PsciOut::Resume;
fn neg(v: i64) -> u64 {
    v as u64
}

#[test]
fn version_and_features() {
    let vm = Vm::new();
    assert_eq!(call(&vm, [0x8400_0000, 0, 0, 0]), (0x10000, E));
    assert_eq!(call(&vm, [0x8000_0000, 0, 0, 0]), (0x10001, E));
    assert_eq!(call(&vm, [0x8400_0006, 0, 0, 0]), (2, E));
    for f in [0x8400_0000u64, 0xC400_0003, 0x8400_0003, 0x8400_0002, 0xC400_0004, 0x8400_0008, 0x8400_0009, 0x8400_000A, 0x8400_0006, 0xC400_0001, 0x8000_0000] {
        assert_eq!(call(&vm, [0x8400_000A, f, 0, 0]), (0, E), "{f:#x}");
    }
    assert_eq!(call(&vm, [0x8400_000A, 0x8400_000E, 0, 0]), (0xffff_ffff, E));
    assert_eq!(call(&vm, [0x8000_0001, 0x8000_0000, 0, 0]), (0, E));
    assert_eq!(call(&vm, [0x8000_0001, 0x8000_8000, 0, 0]), (0xffff_ffff, E));
}

#[test]
fn unknown_and_aliases() {
    let vm = Vm::new();
    // 32-bit return is a zero-extended w0
    assert_eq!(call(&vm, [0x8400_00ff, 0, 0, 0]), (0xffff_ffff, E));
    assert_eq!(call(&vm, [0xC400_00ff, 0, 0, 0]), (neg(-1), E));
    // SMC64 alias of VERSION/CPU_OFF/SYSTEM_OFF does not exist
    assert_eq!(call(&vm, [0xC400_0000, 0, 0, 0]), (neg(-1), E));
    assert_eq!(call(&vm, [0xC400_0008, 0, 0, 0]), (neg(-1), E));
    // SYSTEM_SUSPEND / MIGRATE not provided
    assert_eq!(call(&vm, [0xC400_000E, 0, 0, 0]), (neg(-1), E));
    assert_eq!(call(&vm, [0x8400_0005, 0, 0, 0]), (0xffff_ffff, E));
}

#[test]
fn cpu_on() {
    let vm = Vm::new();
    assert_eq!(call(&vm, [0xC400_0003, 0x8000_0001, 0x4000_0000, 0x55]), (0, E));
    assert_eq!(*vm.started.borrow(), vec![(1, 0x4000_0000, 0x55)]);
    assert_eq!(call(&vm, [0xC400_0003, 0x8000_0001, 0, 0]), (neg(-5), E)); // pending now
    assert_eq!(call(&vm, [0xC400_0003, 0x8000_0000, 0, 0]), (neg(-4), E)); // already on
    assert_eq!(call(&vm, [0xC400_0003, 0x8000_0009, 0, 0]), (neg(-2), E)); // no such cpu
    // SMC32: arguments are truncated to 32 bits; result sign-extended to w0 only
    assert_eq!(call(&vm, [0x8400_0003, 0xdead_0000_8000_0003, 0x1_0000_1000, 7]), (0, E));
    assert_eq!(vm.started.borrow()[1], (3, 0x1000, 7));
    // Aff3 ignored bits above are not compared, RES bits in 31:24 are
    assert_eq!(call(&vm, [0xC400_0003, 0x1_8000_0002, 0, 0]), (neg(-2), E));
}

#[test]
fn affinity_info() {
    let vm = Vm::new();
    assert_eq!(call(&vm, [0xC400_0004, 0x8000_0000, 0, 0]), (0, E)); // on
    assert_eq!(call(&vm, [0xC400_0004, 0x8000_0001, 0, 0]), (1, E)); // off
    assert_eq!(call(&vm, [0xC400_0004, 0x8000_0002, 0, 0]), (2, E)); // on pending
    assert_eq!(call(&vm, [0xC400_0004, 0x8000_0000, 1, 0]), (0, E)); // cluster: any on
    assert_eq!(call(&vm, [0xC400_0004, 0x8000_0000, 4, 0]), (neg(-2), E));
    assert_eq!(call(&vm, [0xC400_0004, 0x8000_0007, 0, 0]), (neg(-2), E));
    assert_eq!(call(&vm, [0x8400_0004, 0x8000_0003, 0, 0]), (1, E));
}

#[test]
fn power_calls() {
    let vm = Vm::new();
    assert_eq!(call(&vm, [0x8400_0002, 0, 0, 0]).1, PsciOut::CpuOff);
    assert_eq!(call(&vm, [0x8400_0008, 0, 0, 0]).1, PsciOut::SystemOff);
    assert_eq!(call(&vm, [0x8400_0009, 0, 0, 0]).1, PsciOut::SystemReset);
    assert_eq!(call(&vm, [0xC400_0001, 0, 0, 0]), (0, PsciOut::Wfi));
    // arguments x1..x3 are preserved
    let mut x = [0x8400_0000, 1, 2, 3];
    handle(&vm, 0, &mut x);
    assert_eq!(&x[1..], &[1, 2, 3]);
}
