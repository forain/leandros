// Exception vectors installed in VBAR_EL2 while a guest runs. The host's own
// table also serves host EL0, so the lower-EL slots cannot be shared. Only the
// lower-EL AArch64 slots are live (HCR.RW=1); every other slot is a fault of
// the world switch itself and parks the CPU.

.macro GUEST_EXIT kind
    .balign 128
    stp  x0, x1, [sp, #-16]!
    mov  x1, #\kind
    b    kvm_exit_common
.endm

.macro PARK
    .balign 128
1:  wfe
    b    1b
.endm

    .text
    .balign 2048
    .global kvm_guest_vectors
kvm_guest_vectors:
    PARK            // current EL, SP0: sync / irq / fiq / serror
    PARK
    PARK
    PARK
    PARK            // current EL, SPx
    PARK
    PARK
    PARK
    GUEST_EXIT 0    // lower EL A64 sync
    GUEST_EXIT 1    // irq
    GUEST_EXIT 2    // fiq
    GUEST_EXIT 3    // serror
    PARK            // lower EL A32 (RW=1, never taken)
    PARK
    PARK
    PARK
