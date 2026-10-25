//! Edge cases for the kernel's x86_64 mem* bodies: every length 0..=LEN, every
//! source/destination misalignment 0..8, overlap in both directions at every
//! distance, and DF left clear afterwards. Compared against a byte reference.
//! The same cases run against kernel/src/mem_a64.rs on an aarch64 host.

#[cfg(target_arch = "x86_64")]
#[path = "../../src/mem_x86.rs"]
mod x86;
#[cfg(target_arch = "aarch64")]
#[path = "../../src/mem_a64.rs"]
mod x86;

const LEN: usize = 80;
const BUF: usize = 3 * LEN + 32;

fn pattern(seed: u8) -> Vec<u8> {
    (0..BUF).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) ^ 0x5a).collect()
}

#[cfg(not(target_arch = "x86_64"))]
fn df_clear() -> bool { true }

#[cfg(target_arch = "x86_64")]
fn df_clear() -> bool {
    let f: u64;
    unsafe { core::arch::asm!("pushfq", "pop {}", out(reg) f) };
    f & (1 << 10) == 0
}

#[test]
fn memset_all_lengths_and_alignments() {
    for len in 0..=LEN {
        for off in 0..8 {
            let mut got = pattern(1);
            let mut want = got.clone();
            let base = 16 + off;
            let r = unsafe { x86::fill(got.as_mut_ptr().add(base), 0xA7, len) };
            assert_eq!(r, unsafe { got.as_mut_ptr().add(base) });
            want[base..base + len].fill(0xA7);
            assert_eq!(got, want, "memset len {len} off {off}");
            assert!(df_clear());
        }
    }
}

#[test]
fn memcpy_disjoint_all_lengths_and_alignments() {
    for len in 0..=LEN {
        for so in 0..8 {
            for d_o in 0..8 {
                let src = pattern(7);
                let mut got = pattern(3);
                let mut want = got.clone();
                let (s, d) = (8 + so, 8 + d_o);
                unsafe { x86::copy_forward(got.as_mut_ptr().add(d), src.as_ptr().add(s), len) };
                want[d..d + len].copy_from_slice(&src[s..s + len]);
                assert_eq!(got, want, "memcpy len {len} so {so} do {d_o}");
                assert!(df_clear());
            }
        }
    }
}

#[test]
fn memmove_overlap_both_directions() {
    for len in 0..=LEN {
        for base in 16..24 {
            // dest - src from -(LEN+2) to +(LEN+2): below, overlapping both ways, equal, disjoint.
            for delta in -(LEN as isize + 2)..=(LEN as isize + 2) {
                let s = (LEN as isize + base as isize) as usize;
                let d = (s as isize + delta) as usize;
                let mut got = pattern(9);
                let mut want = got.clone();
                let tmp: Vec<u8> = want[s..s + len].to_vec();
                want[d..d + len].copy_from_slice(&tmp);
                let p = got.as_mut_ptr();
                let r = unsafe { x86::copy_overlapping(p.add(d), p.add(s), len) };
                assert_eq!(r, unsafe { p.add(d) });
                assert_eq!(got, want, "memmove len {len} src {s} delta {delta}");
                assert!(df_clear(), "DF left set: len {len} delta {delta}");
            }
        }
    }
}

#[test]
fn large_copies() {
    for len in [4095usize, 4096, 4097, 65536 + 3] {
        let src: Vec<u8> = (0..len + 16).map(|i| (i * 7 + 1) as u8).collect();
        let mut dst = vec![0u8; len + 16];
        unsafe { x86::copy_forward(dst.as_mut_ptr().add(1), src.as_ptr().add(3), len) };
        assert_eq!(&dst[1..1 + len], &src[3..3 + len]);
        assert_eq!(dst[0], 0);
        assert_eq!(dst[len + 1], 0);
        let mut buf = src.clone();
        let p = buf.as_mut_ptr();
        unsafe { x86::copy_overlapping(p.add(5), p.add(1), len) };
        assert_eq!(&buf[5..5 + len], &src[1..1 + len]);
    }
}
