//! Memory utility functions for kernel linkage

#[cfg(not(target_arch = "x86_64"))]
#[no_mangle]
pub unsafe extern "C" fn memset(dest: *mut u8, val: i32, count: usize) -> *mut u8 {
    let val = val as u8;
    let mut ptr = dest;
    for _ in 0..count {
        *ptr = val;
        ptr = ptr.add(1);
    }
    dest
}

#[cfg(not(target_arch = "x86_64"))]
#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    let mut dst_ptr = dest;
    let mut src_ptr = src;
    for _ in 0..count {
        *dst_ptr = *src_ptr;
        dst_ptr = dst_ptr.add(1);
        src_ptr = src_ptr.add(1);
    }
    dest
}

#[cfg(not(target_arch = "x86_64"))]
#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    if dest == src as *mut u8 {
        return dest;
    }

    if dest < src as *mut u8 {
        // Forward copy
        memcpy(dest, src, count)
    } else {
        // Backward copy
        let mut dst_ptr = dest.add(count - 1);
        let mut src_ptr = src.add(count - 1);
        for _ in 0..count {
            *dst_ptr = *src_ptr;
            if dst_ptr != dest {
                dst_ptr = dst_ptr.sub(1);
            }
            if src_ptr != src {
                src_ptr = src_ptr.sub(1);
            }
        }
        dest
    }
}


// x86_64: string instructions, eight bytes per iteration.
//
// These were byte loops on every arch. On x86_64 that made each kernel copy
// one memory access per byte, which is the whole cost when the destination is
// device memory: the framebuffer console's shadow -> linear-surface blit (a
// full 1920x1080x4 surface after every scroll) is a `copy_nonoverlapping` into
// the boot framebuffer, and under QEMU TCG every store there is a separate
// access to the VGA BAR. Measured on x86_64/TCG (openbugs #69, lane
// inloadx86): one full-surface blit took ~160 ms (~52 MB/s), 90% of the time
// a console `write(2)` spent, capping `echo` loops at ~40 lines/s.
//
// `cld` is explicit rather than assumed: SYSCALL clears DF through FMASK, but
// an interrupt or exception taken from user mode does not, and a backward
// `rep movs` is silent memory corruption.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn memset(dest: *mut u8, val: i32, count: usize) -> *mut u8 {
    let pat = (val as u8 as u64).wrapping_mul(0x0101_0101_0101_0101);
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

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
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

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    // A forward copy is safe whenever dest is below src (or the ranges are
    // disjoint), at any chunk size: each store lands below every byte that
    // is still to be read.
    if (dest as usize).wrapping_sub(src as usize) >= count {
        return memcpy(dest, src, count);
    }
    // dest overlaps above src: copy from the top down. Whole quadwords
    // first (the high end), then the remaining low bytes.
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

#[no_mangle]
pub unsafe extern "C" fn memcmp(s1: *const u8, s2: *const u8, count: usize) -> i32 {
    let mut ptr1 = s1;
    let mut ptr2 = s2;
    for _ in 0..count {
        let a = *ptr1;
        let b = *ptr2;
        if a != b {
            return if a < b { -1 } else { 1 };
        }
        ptr1 = ptr1.add(1);
        ptr2 = ptr2.add(1);
    }
    0
}