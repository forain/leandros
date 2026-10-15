//! Small shared helpers: signal quality, channel/frequency, UUIDs, C strings.

/// The bytes of a NUL-padded fixed buffer up to the first NUL.
pub fn cstr(buf: &[u8]) -> &[u8] {
    let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    &buf[..n]
}

/// Copy `s` into a NUL-padded fixed buffer; false when it does not fit
/// (`s.len() > N`; a full-length name has no terminator, as IFNAMSIZ allows
/// only 15 + NUL, callers keep names <= 15).
pub fn set_cstr<const N: usize>(buf: &mut [u8; N], s: &[u8]) -> bool {
    if s.len() > N { return false; }
    *buf = [0; N];
    buf[..s.len()].copy_from_slice(s);
    true
}

/// NetworkManager's signal quality (0..=100) from dBm
/// (`nm_wifi_utils_level_to_quality`): clamp to [-100, -40], linear.
pub fn quality_from_dbm(dbm: i32) -> u8 {
    if dbm >= 0 {
        // Already a percentage (some drivers report quality, not dBm).
        return dbm.min(100) as u8;
    }
    let v = dbm.clamp(-100, -40);
    (100 - (100 * (-40 - v)) / 60) as u8
}

/// Centre frequency in MHz of an 802.11 channel, or 0 when invalid.
pub fn channel_to_freq(channel: u8, band: u8) -> u16 {
    let ch = channel as u16;
    match band {
        crate::types::band::GHZ_2_4 => match ch {
            14 => 2484,
            1..=13 => 2407 + 5 * ch,
            _ => 0,
        },
        crate::types::band::GHZ_5 => if ch == 0 { 0 } else { 5000 + 5 * ch },
        crate::types::band::GHZ_6 => match ch {
            2 => 5935,
            1..=233 => 5950 + 5 * ch,
            _ => 0,
        },
        _ => 0,
    }
}

/// (channel, band) of a centre frequency, (0, 0) when unknown.
pub fn freq_to_channel(freq: u16) -> (u8, u8) {
    use crate::types::band;
    match freq {
        2484 => (14, band::GHZ_2_4),
        2412..=2472 => (((freq - 2407) / 5) as u8, band::GHZ_2_4),
        5935 => (2, band::GHZ_6),
        5955..=7115 => (((freq - 5950) / 5) as u8, band::GHZ_6),
        5000..=5925 => (((freq - 5000) / 5) as u8, band::GHZ_5),
        _ => (0, 0),
    }
}

/// True for an all-zero UUID ("none").
pub fn uuid_is_nil(u: &[u8; 16]) -> bool { u.iter().all(|&b| b == 0) }

/// Canonical lower-case text form, 36 bytes (8-4-4-4-12).
pub fn uuid_to_str(u: &[u8; 16]) -> [u8; 36] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [b'-'; 36];
    let mut o = 0;
    for (i, b) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) { o += 1; }
        out[o] = HEX[(b >> 4) as usize];
        out[o + 1] = HEX[(b & 15) as usize];
        o += 2;
    }
    out
}

/// Parse the canonical text form (either case). Hyphens are required.
pub fn uuid_from_str(s: &[u8]) -> Option<[u8; 16]> {
    if s.len() != 36 { return None; }
    let mut u = [0u8; 16];
    let mut o = 0;
    let mut i = 0;
    while i < 36 {
        if matches!(i, 8 | 13 | 18 | 23) {
            if s[i] != b'-' { return None; }
            i += 1;
            continue;
        }
        let hi = hexval(s[i])?;
        let lo = hexval(s[i + 1])?;
        u[o] = (hi << 4) | lo;
        o += 1;
        i += 2;
    }
    Some(u)
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// A deterministic UUID (RFC 9562 version 8, "custom") from a domain tag and
/// data. Not cryptographic; used where the same input must always give the
/// same uuid without storing it:
/// * [`UUID_DOMAIN_LEGACY`] + SSID: the profile synthesised from
///   /etc/leandros/wifi.conf or `wifi.ssid=` (kernel);
/// * [`UUID_DOMAIN_WIRED`] + MAC: the shim's synthetic "Wired connection 1".
pub fn derive_uuid(domain: &[u8], data: &[u8]) -> [u8; 16] {
    let mut u = [0u8; 16];
    for (lane, chunk) in u.chunks_exact_mut(4).enumerate() {
        // Four independent CRC lanes over (lane, domain, 0, data).
        let mut c = 0xFFFF_FFFFu32 ^ (lane as u32).wrapping_mul(0x9E37_79B9);
        for &b in domain.iter().chain([0u8].iter()).chain(data.iter()) {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        chunk.copy_from_slice(&(!c).to_be_bytes());
    }
    u[6] = (u[6] & 0x0F) | 0x80; // version 8
    u[8] = (u[8] & 0x3F) | 0x80; // RFC 9562 variant
    u
}

pub const UUID_DOMAIN_LEGACY: &[u8] = b"leandros-netctl-legacy";
pub const UUID_DOMAIN_WIRED: &[u8] = b"leandros-netctl-wired";

/// True when `s` can be a WPA passphrase: 8..=63 bytes. (802.11 says
/// printable ASCII; like the pre-netctl driver, any byte is accepted, since
/// PBKDF2 does not care and real networks have non-ASCII passphrases.)
pub fn is_passphrase(s: &[u8]) -> bool {
    (8..=63).contains(&s.len())
}

/// True when `s` is a raw PSK written as 64 hex digits.
pub fn is_hex_psk(s: &[u8]) -> bool {
    s.len() == 64 && s.iter().all(|c| c.is_ascii_hexdigit())
}

/// Whether `secret` is acceptable for `akm` (an empty secret is always
/// acceptable: the profile then fails with NO_SECRET when used).
pub fn secret_valid_for(akm: u8, secret: &[u8]) -> bool {
    use crate::types::akm;
    if secret.is_empty() { return true; }
    if secret.len() > crate::SECRET_MAX { return false; }
    match akm {
        akm::NONE => false,
        akm::PSK | akm::PSK_SHA256 => is_passphrase(secret) || is_hex_psk(secret),
        // SAE passwords are arbitrary bytes; AUTO may end up as SAE, so it
        // takes anything a PSK or an SAE password could be.
        akm::SAE | akm::AUTO => true,
        _ => false,
    }
}
