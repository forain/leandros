//! Host harness: pulls espctl's picard.rs in as-is (`cargo +stable test --release`
//! in this directory; needs mkfs.fat, mcopy and fsck.fat from brew).
pub fn sha256(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(b).iter().map(|x| format!("{:02x}", x)).collect()
}
#[path = "../../src/picard.rs"]
pub mod picard;
