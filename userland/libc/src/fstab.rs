//! Shared `/etc/fstab` line parser used by `userland/init` (post-pivot
//! secondary mounts), `userland/mount` (`mount -a`), and `userland/fstab`
//! (pretty-printing). Standard 6-column format:
//! `<device> <mountpoint> <fstype> <options> <dump> <pass>`, `#`-comments
//! and blank lines skipped. No `alloc` here (leandros-libc is `no_std`
//! without a global allocator), so fields are fixed-size byte buffers.

const FIELD_CAP: usize = 64;

#[derive(Debug, Clone, Copy)]
pub struct FstabEntry {
    pub device: [u8; FIELD_CAP],
    pub device_len: usize,
    pub mountpoint: [u8; FIELD_CAP],
    pub mountpoint_len: usize,
    pub fstype: [u8; FIELD_CAP],
    pub fstype_len: usize,
    pub options: [u8; FIELD_CAP],
    pub options_len: usize,
}

impl FstabEntry {
    pub fn device(&self) -> &str { core::str::from_utf8(&self.device[..self.device_len]).unwrap_or("") }
    pub fn mountpoint(&self) -> &str { core::str::from_utf8(&self.mountpoint[..self.mountpoint_len]).unwrap_or("") }
    pub fn fstype(&self) -> &str { core::str::from_utf8(&self.fstype[..self.fstype_len]).unwrap_or("") }
    pub fn options(&self) -> &str { core::str::from_utf8(&self.options[..self.options_len]).unwrap_or("") }
}

fn fill(dst: &mut [u8; FIELD_CAP], src: &str) -> usize {
    let n = src.len().min(FIELD_CAP);
    dst[..n].copy_from_slice(&src.as_bytes()[..n]);
    n
}

/// Parse one line of `/etc/fstab`. Returns `None` for comments, blank lines,
/// or lines with fewer than 3 whitespace-separated fields.
pub fn parse_line(line: &str) -> Option<FstabEntry> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.split_whitespace();
    let device = fields.next()?;
    let mountpoint = fields.next()?;
    let fstype = fields.next()?;
    let options = fields.next().unwrap_or("defaults");

    let mut entry = FstabEntry {
        device: [0u8; FIELD_CAP], device_len: 0,
        mountpoint: [0u8; FIELD_CAP], mountpoint_len: 0,
        fstype: [0u8; FIELD_CAP], fstype_len: 0,
        options: [0u8; FIELD_CAP], options_len: 0,
    };
    entry.device_len = fill(&mut entry.device, device);
    entry.mountpoint_len = fill(&mut entry.mountpoint, mountpoint);
    entry.fstype_len = fill(&mut entry.fstype, fstype);
    entry.options_len = fill(&mut entry.options, options);
    Some(entry)
}
