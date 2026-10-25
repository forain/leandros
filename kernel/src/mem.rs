//! Memory utility functions for kernel linkage

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