//! The kernel's GNU build-id (sha1, 20 bytes), from the note the linker
//! scripts place between `__build_id_start` and `__build_id_end`.

use alloc::string::String;
use spin::Once;

extern "C" {
    static __build_id_start: u8;
    static __build_id_end: u8;
}

/// The note's desc bytes; empty if the note is absent or malformed.
pub fn build_id() -> &'static [u8] {
    unsafe {
        let s = core::ptr::addr_of!(__build_id_start) as usize;
        let e = core::ptr::addr_of!(__build_id_end) as usize;
        if e < s + 16 { return &[]; }
        let h = s as *const u32;
        let (namesz, descsz, ty) = (h.read() as usize, h.add(1).read() as usize, h.add(2).read());
        let name = s + 12;
        let desc = name + ((namesz + 3) & !3);
        if ty != 3 || desc + descsz > e || descsz == 0 { return &[]; }
        core::slice::from_raw_parts(desc as *const u8, descsz)
    }
}

/// Lowercase hex of the build-id (computed once); "" if absent.
pub fn build_id_hex() -> &'static str {
    static HEX: Once<String> = Once::new();
    HEX.call_once(|| {
        let mut s = String::new();
        for b in build_id() {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((b & 15) as u32, 16).unwrap());
        }
        s
    })
}
