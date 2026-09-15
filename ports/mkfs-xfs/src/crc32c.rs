//! CRC32C (Castagnoli, reflected) — the checksum every XFS v5 metadata block
//! carries.
//!
//! XFS seeds with ~0, runs the buffer with the CRC field zeroed, and stores the
//! bitwise complement of the result as a little-endian u32 (see
//! libxfs/xfs_cksum.h: xfs_start_cksum_update / xfs_end_cksum).

const POLY: u32 = 0x82F6_3B78; // reflected 0x1EDC6F41

const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

pub fn crc32c(seed: u32, data: &[u8]) -> u32 {
    let mut c = seed;
    for &b in data {
        c = TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c
}

/// Zero the CRC field at `crc_off`, checksum the whole buffer, and store the
/// result back. `buf` must be exactly the length XFS checksums for that
/// structure (a sector for SB/AGF/AGI/AGFL, a block for a btree block, an
/// inode for a dinode).
pub fn stamp(buf: &mut [u8], crc_off: usize) {
    buf[crc_off..crc_off + 4].copy_from_slice(&[0, 0, 0, 0]);
    let crc = crc32c(!0u32, buf);
    buf[crc_off..crc_off + 4].copy_from_slice(&(!crc).to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // The standard CRC-32C check value: "123456789" => 0xE3069283
        // (that is the *final* value, i.e. complement of the raw register).
        assert_eq!(!crc32c(!0u32, b"123456789"), 0xE306_9283);
        assert_eq!(!crc32c(!0u32, b""), 0);
    }
}
