//! Guest code for kvmtest, hand-assembled A64 (little-endian words, one per
//! instruction, the instruction in the trailing comment).
//!
//! GENERATED once from the comments with `llvm-mc -triple=aarch64 -show-encoding`
//! and cross-checked in kvm/host-tests/tests/blob.rs (skipped without llvm-mc).
//! Keep the `0xXXXXXXXX, // asm` line shape: the host test parses it.
//!
//! The guest runs with its MMU off (accesses are Device), PSTATE 0x3c5 (EL1h,
//! DAIF masked), at GPA 0x4000_0000. 0x0900_0000 has no memslot, so every
//! access to it is an MMIO exit. Encodings used:
//!   movz  Rd,#imm16{,lsl#s}  sf 10 100101 hw imm16 Rd   (W: 0x52800000, X: 0xd2800000)
//!   movk  Rd,#imm16,lsl#s    sf 11 100101 hw imm16 Rd   (W: 0x72800000)
//!   str/ldr Wt,[Xn,#imm]     0xb9000000 / 0xb9400000 | imm/4<<10 | Rn<<5 | Rt
//!   hvc #0 = 0xd4000002, wfi = 0xd503207f, b . = 0x14000000
//!   mrs/msr  0xd5300000 / 0xd5100000 | (o0=1)<<19 | op1<<16 | CRn<<12 | CRm<<8 | op2<<5 | Rt
//!   add Xd,Xn,Xm,lsr#i       0x8b400000 | Xm<<16 | i<<10 | Xn<<5 | Xd
//!   add Xd,Xn,#imm{,lsl#12}  0x91000000 | (sh<<22) | imm12<<10 | Xn<<5 | Xd
/// mmio: store 0x42 to an unbacked IPA (write exit), load from it (read exit, host supplies 0x1234), store the loaded value back
pub static MMIO: [u32; 6] = [
    0xd2a12001, // movz x1, #0x900, lsl #16
    0x52800842, // movz w2, #0x42
    0xb9000022, // str w2, [x1]
    0xb9400423, // ldr w3, [x1, #4]
    0xb9000823, // str w3, [x1, #8]
    0x14000000, // b .
];

/// psci: PSCI_VERSION (0x84000000) and an unknown call (0x8400ffff) over HVC, report both results by MMIO
pub static PSCI: [u32; 10] = [
    0xd2a12001, // movz x1, #0x900, lsl #16
    0x52800000, // movz w0, #0
    0x72b08000, // movk w0, #0x8400, lsl #16
    0xd4000002, // hvc #0
    0xb9000820, // str w0, [x1, #8]
    0x529fffe0, // movz w0, #0xffff
    0x72b08000, // movk w0, #0x8400, lsl #16
    0xd4000002, // hvc #0
    0xb9000c20, // str w0, [x1, #12]
    0x14000000, // b .
];

/// wfi: arm CNTV for about 1 ms, WFI, then disable the timer and report 0x57 by MMIO
pub static WFI: [u32; 13] = [
    0xd53be044, // mrs x4, cntvct_el0
    0xd53be005, // mrs x5, cntfrq_el0
    0x8b452884, // add x4, x4, x5, lsr #10
    0xd51be344, // msr cntv_cval_el0, x4
    0xd2800026, // movz x6, #1
    0xd51be326, // msr cntv_ctl_el0, x6
    0xd503207f, // wfi
    0xd2800006, // movz x6, #0
    0xd51be326, // msr cntv_ctl_el0, x6
    0xd2a12001, // movz x1, #0x900, lsl #16
    0x52800ae2, // movz w2, #0x57
    0xb9000c22, // str w2, [x1, #12]
    0x14000000, // b .
];

/// sysoff: PSCI SYSTEM_OFF (0x84000008)
pub static SYSOFF: [u32; 4] = [
    0x52800100, // movz w0, #8
    0x72b08000, // movk w0, #0x8400, lsl #16
    0xd4000002, // hvc #0
    0x14000000, // b .
];

/// spin: branch to self (sigkick, immexit)
pub static SPIN: [u32; 1] = [
    0x14000000, // b .
];

/// memslot: store 0xbabecafe at RAM+0x1000, then SYSTEM_OFF
pub static MEMSLOT: [u32; 9] = [
    0xd2a80001, // movz x1, #0x4000, lsl #16
    0x91401021, // add x1, x1, #4, lsl #12
    0x52995fc2, // movz w2, #0xcafe
    0x72b757c2, // movk w2, #0xbabe, lsl #16
    0xb9000022, // str w2, [x1]
    0x52800100, // movz w0, #8
    0x72b08000, // movk w0, #0x8400, lsl #16
    0xd4000002, // hvc #0
    0x14000000, // b .
];

/// smp vcpu0: PSCI CPU_ON(target 1, entry RAM+0x100, ctx 0x77), report the result by MMIO
pub static SMP0: [u32; 10] = [
    0xd2a12009, // movz x9, #0x900, lsl #16
    0x52800060, // movz w0, #3
    0x72b88000, // movk w0, #0xc400, lsl #16
    0xd2800021, // movz x1, #1
    0xd2a80002, // movz x2, #0x4000, lsl #16
    0x91040042, // add x2, x2, #0x100
    0xd2800ee3, // movz x3, #0x77
    0xd4000002, // hvc #0
    0xb9000d20, // str w0, [x9, #12]
    0x14000000, // b .
];

/// smp vcpu1 (entry RAM+0x100): x0 = context id; report it by MMIO
pub static SMP1: [u32; 3] = [
    0xd2a12009, // movz x9, #0x900, lsl #16
    0xb9001120, // str w0, [x9, #16]
    0x14000000, // b .
];
