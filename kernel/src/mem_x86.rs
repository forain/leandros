//! x86_64 bodies of the kernel's memcpy/memset/memmove: string instructions,
//! eight bytes per iteration plus a byte tail.
//!
//! These were byte loops on every arch. On x86_64 that made each kernel copy
//! one memory access per byte, which is the whole cost when the destination is
//! device memory (the framebuffer console blit under QEMU TCG went at ~52 MB/s,
//! openbugs #69) and a large share of every bulk copy elsewhere.
//!
//! `cld` is explicit rather than assumed: SYSCALL clears DF through FMASK, but
//! an interrupt or exception taken from user mode does not, and a backward
//! `rep movs` is silent memory corruption.
//!
//! No function here may call memcpy/memset/memmove (they ARE those symbols in
//! the kernel). Tested on the host by kernel/memtest (all lengths 0..=80, every
//! misalignment, overlap in both directions).

#[inline(always)]
pub unsafe fn fill(dest: *mut u8, val: u8, count: usize) -> *mut u8 {
    let pat = (val as u64).wrapping_mul(0x0101_0101_0101_0101);
    core::arch::asm!(
        "cld",
        "rep stosq",
        "mov rcx, {rem}",
        "rep stosb",
        rem = in(reg) count & 7,
        inout("rcx") count >> 3 => _,
        inout("rdi") dest => _,
        in("rax") pat,
        options(nostack),
    );
    dest
}

#[inline(always)]
pub unsafe fn copy_forward(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    core::arch::asm!(
        "cld",
        "rep movsq",
        "mov rcx, {rem}",
        "rep movsb",
        rem = in(reg) count & 7,
        inout("rcx") count >> 3 => _,
        inout("rdi") dest => _,
        inout("rsi") src => _,
        options(nostack),
    );
    dest
}

#[inline(always)]
pub unsafe fn copy_overlapping(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    // A forward copy is safe whenever dest is below src (or the ranges are
    // disjoint), at any chunk size: each quadword is loaded before it is
    // stored, and every store lands below every byte still to be read.
    if (dest as usize).wrapping_sub(src as usize) >= count {
        return copy_forward(dest, src, count);
    }
    // dest overlaps above src: copy from the top down. Whole quadwords
    // first (the high end), then the remaining low bytes, then restore DF.
    let q = count >> 3;
    let rem = count & 7;
    core::arch::asm!(
        "std",
        "rep movsq",
        "mov rcx, {rem}",
        "mov rsi, {s}",
        "mov rdi, {d}",
        "rep movsb",
        "cld",
        rem = in(reg) rem,
        s = in(reg) src.wrapping_add(rem).wrapping_sub(1),
        d = in(reg) dest.wrapping_add(rem).wrapping_sub(1),
        inout("rcx") q => _,
        inout("rdi") dest.wrapping_add(count).wrapping_sub(8) => _,
        inout("rsi") src.wrapping_add(count).wrapping_sub(8) => _,
        options(nostack),
    );
    dest
}
