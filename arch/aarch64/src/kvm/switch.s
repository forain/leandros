// KVM world switch (feature kvm). VHE host at EL2 (E2H=1): *_EL1 names reach
// the host's EL2 registers, the guest's EL1 registers are the *_EL12 names.
// Generic S3_5_Cn_Cm_op2 encodings are used so no -mattr=+vh is needed.
//
// Switch frame (on the host stack, pointed to by TPIDR_EL2 while the guest runs):
//   0 hw  8..104 x19..x30  104 out  112 daif  120 sp_el0  128 tpidr_el1
//   136 tpidr_el2  144 vbar  152 hcr  160 cptr  168 mdcr  176 cnthctl
//   184 mdscr  192 csselr  200 par  208 vmpidr  216 vpidr  224 cntvoff  232 vttbr
//   (240 bytes)
// VcpuHw: gp 0, sp_el0 248, sp_el1 256, pc 264, pstate 272, el1 288.. (El1Regs is align 16; first 26 u64 switched), hcr 1056,
// vttbr 1064, vmpidr 1072, cntvoff 1080 (asserted in ctx.rs).

.equ F_SIZE,   240
.equ F_HW,     0
.equ F_X19,    8
.equ F_OUT,    104
.equ F_DAIF,   112
.equ F_SPEL0,  120
.equ F_TPIDR1, 128
.equ F_TPIDR2, 136
.equ F_VBAR,   144
.equ F_HCR,    152
.equ F_CPTR,   160
.equ F_MDCR,   168
.equ F_CNTHCTL,176
.equ F_MDSCR,  184
.equ F_CSSELR, 192
.equ F_PAR,    200
.equ F_VMPIDR, 208
.equ F_VPIDR,  216
.equ F_CNTVOFF,224
.equ F_VTTBR,  232

.equ HW_SPEL0, 248
.equ HW_SPEL1, 256
.equ HW_PC,    264
.equ HW_PSTATE,272
.equ HW_EL1,   288
.equ HW_HCR,   1056
.equ HW_VTTBR, 1064
.equ HW_VMPIDR,1072
.equ HW_CNTVOFF,1080

// guest constants (see kvm::hw)
.equ G_CPTR,    0x300000
.equ G_MDCR,    0xE60
.equ G_CNTHCTL, 0x400

// Host EL2 state saved to / restored from the frame (sp-relative).
.macro HOST_SAVE reg, off
    mrs x2, \reg
    str x2, [sp, #\off]
.endm
.macro HOST_LOAD reg, off
    ldr x2, [sp, #\off]
    msr \reg, x2
.endm

// Guest EL1 register i at HW_EL1 + 8*i, x0 = VcpuHw.
.macro G_SAVE reg, i
    mrs x2, \reg
    str x2, [x0, #(HW_EL1 + 8*\i)]
.endm
.macro G_LOAD reg, i
    ldr x2, [x0, #(HW_EL1 + 8*\i)]
    msr \reg, x2
.endm

.macro GUEST_EL1_SAVE
    G_SAVE S3_5_C1_C0_0,   0   // sctlr_el12
    G_SAVE S3_5_C1_C0_2,   1   // cpacr_el12
    G_SAVE S3_5_C2_C0_0,   2   // ttbr0_el12
    G_SAVE S3_5_C2_C0_1,   3   // ttbr1_el12
    G_SAVE S3_5_C2_C0_2,   4   // tcr_el12
    G_SAVE S3_5_C5_C2_0,   5   // esr_el12
    G_SAVE S3_5_C5_C1_0,   6   // afsr0_el12
    G_SAVE S3_5_C5_C1_1,   7   // afsr1_el12
    G_SAVE S3_5_C6_C0_0,   8   // far_el12
    G_SAVE S3_5_C10_C2_0,  9   // mair_el12
    G_SAVE S3_5_C10_C3_0, 10   // amair_el12
    G_SAVE S3_5_C12_C0_0, 11   // vbar_el12
    G_SAVE S3_5_C13_C0_1, 12   // contextidr_el12
    G_SAVE S3_5_C14_C1_0, 18   // cntkctl_el12
    G_SAVE S3_5_C4_C0_1, 20   // elr_el12
    G_SAVE S3_5_C4_C0_0, 21   // spsr_el12
    G_SAVE par_el1, 16
    G_SAVE csselr_el1, 17
    G_SAVE mdscr_el1, 19
    G_SAVE tpidr_el0, 14
    G_SAVE tpidrro_el0, 15
    G_SAVE tpidr_el1, 13
.endm

.macro GUEST_EL1_LOAD
    G_LOAD S3_5_C1_C0_0,   0
    G_LOAD S3_5_C1_C0_2,   1
    G_LOAD S3_5_C2_C0_0,   2
    G_LOAD S3_5_C2_C0_1,   3
    G_LOAD S3_5_C2_C0_2,   4
    G_LOAD S3_5_C5_C2_0,   5
    G_LOAD S3_5_C5_C1_0,   6
    G_LOAD S3_5_C5_C1_1,   7
    G_LOAD S3_5_C6_C0_0,   8
    G_LOAD S3_5_C10_C2_0,  9
    G_LOAD S3_5_C10_C3_0, 10
    G_LOAD S3_5_C12_C0_0, 11
    G_LOAD S3_5_C13_C0_1, 12
    G_LOAD S3_5_C14_C1_0, 18
    G_LOAD S3_5_C4_C0_1, 20
    G_LOAD S3_5_C4_C0_0, 21
    G_LOAD par_el1, 16
    G_LOAD csselr_el1, 17
    G_LOAD mdscr_el1, 19
    G_LOAD tpidr_el0, 14
    G_LOAD tpidrro_el0, 15
    G_LOAD tpidr_el1, 13
.endm

    .text
    .balign 4
    // void kvm_enter_raw(VcpuHw *hw /*x0*/, RawExit *out /*x1*/)
    .global kvm_enter_raw
kvm_enter_raw:
    sub  sp, sp, #F_SIZE
    stp  x19, x20, [sp, #(F_X19 + 0)]
    stp  x21, x22, [sp, #(F_X19 + 16)]
    stp  x23, x24, [sp, #(F_X19 + 32)]
    stp  x25, x26, [sp, #(F_X19 + 48)]
    stp  x27, x28, [sp, #(F_X19 + 64)]
    stp  x29, x30, [sp, #(F_X19 + 80)]
    str  x0, [sp, #F_HW]
    str  x1, [sp, #F_OUT]
    HOST_SAVE daif,        F_DAIF
    msr  daifset, #0xf
    HOST_SAVE sp_el0,      F_SPEL0
    HOST_SAVE tpidr_el1,   F_TPIDR1
    HOST_SAVE tpidr_el2,   F_TPIDR2
    HOST_SAVE vbar_el2,    F_VBAR
    HOST_SAVE hcr_el2,     F_HCR
    HOST_SAVE cptr_el2,    F_CPTR
    HOST_SAVE mdcr_el2,    F_MDCR
    HOST_SAVE cnthctl_el2, F_CNTHCTL
    HOST_SAVE mdscr_el1,   F_MDSCR
    HOST_SAVE csselr_el1,  F_CSSELR
    HOST_SAVE par_el1,     F_PAR
    HOST_SAVE vmpidr_el2,  F_VMPIDR
    HOST_SAVE vpidr_el2,   F_VPIDR
    HOST_SAVE cntvoff_el2, F_CNTVOFF
    HOST_SAVE vttbr_el2,   F_VTTBR
    mov  x2, sp
    msr  tpidr_el2, x2

    // Stage 2 and virtual IDs first, then the EL1 file, HCR (TGE=0) last.
    ldr  x2, [x0, #HW_VTTBR]
    msr  vttbr_el2, x2
    ldr  x2, [x0, #HW_VMPIDR]
    msr  vmpidr_el2, x2
    mrs  x2, midr_el1
    msr  vpidr_el2, x2
    ldr  x2, [x0, #HW_CNTVOFF]
    msr  cntvoff_el2, x2
    GUEST_EL1_LOAD
    ldr  x2, [x0, #HW_SPEL0]
    msr  sp_el0, x2
    ldr  x2, [x0, #HW_SPEL1]
    msr  sp_el1, x2
    ldr  x2, [x0, #HW_PC]
    msr  elr_el2, x2
    ldr  x2, [x0, #HW_PSTATE]
    msr  spsr_el2, x2
    mov  x2, #G_CPTR
    msr  cptr_el2, x2
    mov  x2, #G_MDCR
    msr  mdcr_el2, x2
    mov  x2, #G_CNTHCTL
    msr  cnthctl_el2, x2
    adrp x2, kvm_guest_vectors
    add  x2, x2, :lo12:kvm_guest_vectors
    msr  vbar_el2, x2
    ldr  x2, [x0, #HW_HCR]
    msr  hcr_el2, x2
    isb

    ldp  x2, x3,   [x0, #16]
    ldp  x4, x5,   [x0, #32]
    ldp  x6, x7,   [x0, #48]
    ldp  x8, x9,   [x0, #64]
    ldp  x10, x11, [x0, #80]
    ldp  x12, x13, [x0, #96]
    ldp  x14, x15, [x0, #112]
    ldp  x16, x17, [x0, #128]
    ldp  x18, x19, [x0, #144]
    ldp  x20, x21, [x0, #160]
    ldp  x22, x23, [x0, #176]
    ldp  x24, x25, [x0, #192]
    ldp  x26, x27, [x0, #208]
    ldp  x28, x29, [x0, #224]
    ldr  x30, [x0, #240]
    ldr  x1, [x0, #8]
    ldr  x0, [x0, #0]
    eret
    dsb  nsh
    isb

    // Common exit path. Entered from kvm_guest_vectors with SP = the host stack
    // (SP_EL2, 16 bytes holding guest x0,x1 pushed), x1 = exit kind.
    .global kvm_exit_common
kvm_exit_common:
    mrs  x0, tpidr_el2             // frame
    ldr  x0, [x0, #F_HW]           // x0 = VcpuHw
    stp  x2, x3,   [x0, #16]
    ldp  x2, x3,   [sp], #16       // guest x0, x1
    stp  x2, x3,   [x0, #0]
    stp  x4, x5,   [x0, #32]
    stp  x6, x7,   [x0, #48]
    stp  x8, x9,   [x0, #64]
    stp  x10, x11, [x0, #80]
    stp  x12, x13, [x0, #96]
    stp  x14, x15, [x0, #112]
    stp  x16, x17, [x0, #128]
    stp  x18, x19, [x0, #144]
    stp  x20, x21, [x0, #160]
    stp  x22, x23, [x0, #176]
    stp  x24, x25, [x0, #192]
    stp  x26, x27, [x0, #208]
    stp  x28, x29, [x0, #224]
    str  x30, [x0, #240]

    // Spectre-BHB (Cortex-A76, k=24 loop as Linux): overwrite the branch history
    // the guest trained before any host indirect branch runs. Guest x0-x30 are saved.
    // TODO(kvm-run review): use CLRBHB where ID_AA64ISAR2.CLRBHB is set, and an
    // SMCCC_ARCH_WORKAROUND_3 call where firmware needs one; CSV2==0 hosts also need
    // a branch-predictor invalidate here (BCM2712 is CSV2>=1, check on the card).
    mov  x2, #24
8:  b    9f
9:  subs x2, x2, #1
    b.ne 8b
    dsb  nsh
    isb

    mrs  x2, sp_el0
    str  x2, [x0, #HW_SPEL0]
    mrs  x2, sp_el1
    str  x2, [x0, #HW_SPEL1]
    mrs  x2, elr_el2
    str  x2, [x0, #HW_PC]
    mrs  x2, spsr_el2
    str  x2, [x0, #HW_PSTATE]
    GUEST_EL1_SAVE

    // syndrome -> RawExit
    mrs  x4, tpidr_el2
    ldr  x5, [x4, #F_OUT]
    mrs  x2, esr_el2
    mrs  x3, far_el2
    mrs  x6, hpfar_el2
    strb w1, [x5, #0]
    str  x2, [x5, #8]
    str  x3, [x5, #16]
    str  x6, [x5, #24]

    // Back to the host: TGE=1 first, then the rest of EL2 state.
    mov  sp, x4
    HOST_LOAD hcr_el2,     F_HCR
    isb
    HOST_LOAD vbar_el2,    F_VBAR
    HOST_LOAD cptr_el2,    F_CPTR
    HOST_LOAD mdcr_el2,    F_MDCR
    HOST_LOAD cnthctl_el2, F_CNTHCTL
    HOST_LOAD vttbr_el2,   F_VTTBR
    HOST_LOAD vmpidr_el2,  F_VMPIDR
    HOST_LOAD vpidr_el2,   F_VPIDR
    HOST_LOAD cntvoff_el2, F_CNTVOFF
    HOST_LOAD sp_el0,      F_SPEL0
    HOST_LOAD tpidr_el1,   F_TPIDR1
    HOST_LOAD mdscr_el1,   F_MDSCR
    HOST_LOAD csselr_el1,  F_CSSELR
    HOST_LOAD par_el1,     F_PAR
    HOST_LOAD tpidr_el2,   F_TPIDR2
    HOST_LOAD daif,        F_DAIF
    ldp  x19, x20, [sp, #(F_X19 + 0)]
    ldp  x21, x22, [sp, #(F_X19 + 16)]
    ldp  x23, x24, [sp, #(F_X19 + 32)]
    ldp  x25, x26, [sp, #(F_X19 + 48)]
    ldp  x27, x28, [sp, #(F_X19 + 64)]
    ldp  x29, x30, [sp, #(F_X19 + 80)]
    add  sp, sp, #F_SIZE
    isb
    ret
