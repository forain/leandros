//! Memory utility functions for kernel linkage

// aarch64: aligned doubleword bulk + byte head/tail, see mem_a64.rs (shared
// with the host test kernel/memtest, which runs it natively on Apple silicon).
#[cfg(target_arch = "aarch64")]
#[path = "mem_a64.rs"]
mod a64;

#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub unsafe extern "C" fn memset(dest: *mut u8, val: i32, count: usize) -> *mut u8 {
    a64::fill(dest, val as u8, count)
}

#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    a64::copy_forward(dest, src, count)
}

#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    a64::copy_overlapping(dest, src, count)
}

// x86_64: string instructions (rep movsq/stosq + byte tail), see mem_x86.rs.
// Kept in its own file so the host test (kernel/memtest, run under Rosetta)
// exercises exactly this code.
#[cfg(target_arch = "x86_64")]
#[path = "mem_x86.rs"]
mod x86;

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn memset(dest: *mut u8, val: i32, count: usize) -> *mut u8 {
    x86::fill(dest, val as u8, count)
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    x86::copy_forward(dest, src, count)
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    x86::copy_overlapping(dest, src, count)
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