//! Writing the installed ESP: FAT32 format, recursive copy of the boot ESP
//! tree with every `*.conf` rewritten (root=PARTUUID, no usbms.*, nvme.rw=1
//! for NVMe), then read-back verification by SHA-256.

use crate::dev::Dev;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};

type Fs = fatfs::FileSystem<Dev>;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn err<T>(m: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Other, m.into()))
}

/// Rewrite one cmdline value.
fn fix_cmdline(val: &str, root_partuuid: &str, extra: Option<&str>) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut seen_root = false;
    for t in val.split_whitespace() {
        if t.starts_with("usbms.") {
            continue;
        }
        // A caller-supplied write policy replaces any inherited one.
        if extra.is_some() && (t.starts_with("blk.rw=") || t == "nvme.rw=1") {
            continue;
        }
        if t.starts_with("root=") {
            if !seen_root {
                out.push(format!("root=PARTUUID={}", root_partuuid));
                seen_root = true;
            }
            continue;
        }
        out.push(t.to_string());
    }
    if !seen_root {
        out.insert(0, format!("root=PARTUUID={}", root_partuuid));
    }
    if let Some(x) = extra {
        if !out.iter().any(|t| t == x) {
            out.push(x.to_string());
        }
    }
    out.join(" ")
}

fn is_cmdline_key(line: &str) -> Option<(usize, usize)> {
    // (offset of the value, length of the key part incl. colon) for `cmdline:` /
    // `kernel_cmdline:` lines (the key is case-insensitive in limine.conf).
    let t = line.trim_start();
    let ind = line.len() - t.len();
    let (k, _) = t.split_once(':')?;
    let k = k.trim().to_ascii_lowercase();
    if k == "cmdline" || k == "kernel_cmdline" {
        let colon = ind + t.find(':').unwrap();
        Some((ind, colon + 1))
    } else {
        None
    }
}

/// Rewrite a limine.conf: every entry's `cmdline:` gets root=PARTUUID=<root>,
/// loses `usbms.*`, gains `nvme.rw=1` on NVMe; an entry that boots a kernel
/// (`path:` and not `protocol: efi`) without a cmdline gets one.
pub fn rewrite_conf(text: &str, root_partuuid: &str, nvme: bool) -> String {
    rewrite_conf_rw(text, root_partuuid, if nvme { Some("nvme.rw=1") } else { None })
}

/// Like `rewrite_conf`, with an explicit extra cmdline token (`blk.rw=...`)
/// that also replaces any inherited `blk.rw=` / `nvme.rw=1`.
pub fn rewrite_conf_rw(text: &str, root_partuuid: &str, nvme: Option<&str>) -> String {
    let mut out: Vec<String> = Vec::new();
    // Start index in `out` of the current entry body, whether it has a
    // cmdline, a kernel path, or is an EFI chain entry.
    let mut cur: Option<(usize, bool, bool, bool)> = None;
    fn close(out: &mut Vec<String>, cur: &mut Option<(usize, bool, bool, bool)>, root: &str, nvme: Option<&str>) {
        if let Some((_start, has_cmd, has_path, efi)) = cur.take() {
            if has_path && !has_cmd && !efi {
                let mut at = out.len();
                while at > 0 && out[at - 1].trim().is_empty() {
                    at -= 1;
                }
                out.insert(at, format!("\tcmdline: {}", fix_cmdline("", root, nvme)));
            }
        }
    }
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('/') && !line.starts_with("//") {
            close(&mut out, &mut cur, root_partuuid, nvme);
            cur = Some((out.len(), false, false, false));
            out.push(line.to_string());
            continue;
        }
        if let Some((_, has_cmd, has_path, efi)) = cur.as_mut() {
            let t = line.trim();
            let tl = t.to_ascii_lowercase();
            if let Some((_ind, kend)) = is_cmdline_key(line) {
                *has_cmd = true;
                let val = &line[kend..];
                out.push(format!("{} {}", &line[..kend], fix_cmdline(val, root_partuuid, nvme)));
                continue;
            }
            if tl.starts_with("path:") || tl.starts_with("kernel_path:") {
                *has_path = true;
            }
            if tl.starts_with("protocol:") && tl.contains("efi") {
                *efi = true;
            }
        }
        out.push(line.to_string());
    }
    close(&mut out, &mut cur, root_partuuid, nvme);
    let mut s = out.join("\n");
    s.push('\n');
    s
}

fn norm_join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{}/{}", dir, name) }
}

/// Files of the source tree: (path, is_dir).
fn walk(fs: &Fs, dir: &str, out: &mut Vec<(String, bool, u64)>) -> io::Result<()> {
    let d = if dir.is_empty() { fs.root_dir() } else { fs.root_dir().open_dir(dir)? };
    let mut ents: Vec<(String, bool, u64)> = Vec::new();
    for e in d.iter() {
        let e = e?;
        let n = e.file_name();
        if n == "." || n == ".." {
            continue;
        }
        ents.push((n, e.is_dir(), e.len()));
    }
    ents.sort();
    for (n, is_dir, len) in ents {
        let p = norm_join(dir, &n);
        out.push((p.clone(), is_dir, len));
        if is_dir {
            walk(fs, &p, out)?;
        }
    }
    Ok(())
}

fn is_conf(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".conf")
}

pub struct EspResult {
    pub files: usize,
    pub bytes: u64,
    pub confs: Vec<String>,
}

/// Format the target window as FAT32 "LEANDROS" and copy `src` into it.
/// `progress(pct)` is called as files are copied.
pub fn install_esp(
    src: &Dev,
    dst: &Dev,
    root_partuuid: &str,
    nvme: bool,
    progress: impl FnMut(u32),
) -> io::Result<EspResult> {
    install_esp_ext(src, dst, root_partuuid, if nvme { Some("nvme.rw=1") } else { None }, *b"LEANDROS   ", &[], progress)
}

/// `install_esp` with an explicit extra cmdline token, volume label and extra
/// files (path, content) created after the tree copy (dual-boot: the private
/// boot partition and its `leandros/owner` marker).
pub fn install_esp_ext(
    src: &Dev,
    dst: &Dev,
    root_partuuid: &str,
    extra: Option<&str>,
    label: [u8; 11],
    extra_files: &[(String, Vec<u8>)],
    mut progress: impl FnMut(u32),
) -> io::Result<EspResult> {
    let nvme = extra;
    fatfs::format_volume(
        dst.handle(),
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32).volume_label(label),
    )?;
    dst.sync()?;
    dst.invalidate()?;
    let sfs = fatfs::FileSystem::new(src.handle(), fatfs::FsOptions::new())?;
    let dfs = fatfs::FileSystem::new(dst.handle(), fatfs::FsOptions::new())?;
    let mut tree = Vec::new();
    walk(&sfs, "", &mut tree)?;
    let total: u64 = {
        tree.iter().map(|(_, _, l)| *l).sum::<u64>().max(1)
    };
    let mut done = 0u64;
    let mut res = EspResult { files: 0, bytes: 0, confs: Vec::new() };
    let mut last_pct = 101;
    let mut sums: BTreeMap<String, String> = BTreeMap::new();
    for (p, is_dir, _) in &tree {
        if *is_dir {
            dfs.root_dir().create_dir(p)?;
            continue;
        }
        let mut sf = sfs.root_dir().open_file(p)?;
        let mut df = dfs.root_dir().create_file(p)?;
        df.truncate()?;
        let mut h = Sha256::new();
        if is_conf(p) {
            let mut v = Vec::new();
            sf.read_to_end(&mut v)?;
            let text = rewrite_conf_rw(&String::from_utf8_lossy(&v), root_partuuid, nvme);
            h.update(text.as_bytes());
            df.write_all(text.as_bytes())?;
            done += v.len() as u64;
            res.confs.push(p.clone());
        } else {
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = sf.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                df.write_all(&buf[..n])?;
                done += n as u64;
                let pct = (done * 100 / total) as u32;
                if pct != last_pct {
                    last_pct = pct;
                    progress(pct.min(99));
                }
            }
        }
        df.flush()?;
        drop(df);
        sums.insert(p.clone(), hex(&h.finalize()));
        res.files += 1;
    }
    for (p, data) in extra_files {
        if let Some((dir, _)) = p.rsplit_once('/') {
            let mut acc = String::new();
            for comp in dir.split('/') {
                acc = norm_join(&acc, comp);
                let _ = dfs.root_dir().create_dir(&acc)?;
            }
        }
        let mut df = dfs.root_dir().create_file(p)?;
        df.truncate()?;
        df.write_all(data)?;
        df.flush()?;
        drop(df);
        sums.insert(p.clone(), hex(&Sha256::digest(data)));
    }
    res.bytes = done;
    dfs.unmount()?;
    drop(sfs);
    dst.sync()?;
    // Read back from the medium and compare.
    dst.invalidate()?;
    let vfs = fatfs::FileSystem::new(dst.handle(), fatfs::FsOptions::new())?;
    for (p, want) in &sums {
        let mut f = vfs.root_dir().open_file(p)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        if &hex(&h.finalize()) != want {
            return err(format!("ESP read-back verification failed for {}", p));
        }
    }
    progress(100);
    Ok(res)
}

/// The text of the source ESP's limine.conf and its rewritten form, for
/// `--dry-run`.
pub fn preview_conf(src: &Dev, root_partuuid: &str, nvme: bool) -> io::Result<(String, String)> {
    let fs = fatfs::FileSystem::new(src.handle(), fatfs::FsOptions::new())?;
    let mut f = fs.root_dir().open_file("limine.conf")?;
    let mut v = Vec::new();
    f.read_to_end(&mut v)?;
    let old = String::from_utf8_lossy(&v).into_owned();
    let new = rewrite_conf(&old, root_partuuid, nvme);
    Ok((old, new))
}

/// Like `preview_conf` with an explicit extra cmdline token.
pub fn preview_conf_rw(src: &Dev, root_partuuid: &str, extra: &str) -> io::Result<(String, String)> {
    let fs = fatfs::FileSystem::new(src.handle(), fatfs::FsOptions::new())?;
    let mut f = fs.root_dir().open_file("limine.conf")?;
    let mut v = Vec::new();
    f.read_to_end(&mut v)?;
    let old = String::from_utf8_lossy(&v).into_owned();
    let new = rewrite_conf_rw(&old, root_partuuid, Some(extra));
    Ok((old, new))
}

/// What a dual-boot install adds to the shared ESP: (path, content). Nothing
/// else on the ESP is created, changed or deleted.
pub fn shared_files(efi_name: &str, limine_bin: &[u8], boot_partuuid: &str) -> Vec<(String, Vec<u8>)> {
    let conf = format!(
        "timeout: 0\nserial: yes\n\n/LeandrOS\n\tprotocol: efi\n\tpath: guid({}):/EFI/BOOT/{}\n",
        boot_partuuid, efi_name
    );
    let entry = format!("title LeandrOS\nefi /EFI/leandros/{}\nsort-key zz-leandros\n", efi_name);
    vec![
        (format!("EFI/leandros/{}", efi_name), limine_bin.to_vec()),
        ("EFI/leandros/limine.conf".to_string(), conf.into_bytes()),
        ("loader/entries/leandros.conf".to_string(), entry.into_bytes()),
        ("EFI/leandros/.leandros-owned".to_string(), format!("{}\n", boot_partuuid).into_bytes()),
    ]
}

/// Read-only look at a shared ESP: (fat32, dirty bit set, free bytes,
/// has /loader/entries, has /EFI/leandros, has /loader/entries/leandros.conf).
pub struct EspProbe {
    pub fat32: bool,
    pub dirty: bool,
    pub free_bytes: u64,
    pub has_entries_dir: bool,
    pub has_ours: bool,
}

pub fn probe_shared(dev: &Dev) -> io::Result<EspProbe> {
    let mut bs = [0u8; 512];
    {
        let mut h = dev.handle();
        use std::io::Seek;
        h.seek(io::SeekFrom::Start(0))?;
        h.read_exact(&mut bs)?;
    }
    let fat32 = &bs[0x52..0x5A] == b"FAT32   ";
    let dirty = bs[0x41] & 1 != 0;
    let fs = fatfs::FileSystem::new(dev.handle(), fatfs::FsOptions::new())?;
    let st = fs.stats()?;
    let free_bytes = st.free_clusters() as u64 * st.cluster_size() as u64;
    let root = fs.root_dir();
    let has_entries_dir = root.open_dir("loader/entries").is_ok();
    let has_ours = root.open_dir("EFI/leandros").is_ok() || root.open_file("loader/entries/leandros.conf").is_ok();
    drop(root);
    drop(fs);
    Ok(EspProbe { fat32, dirty, free_bytes, has_entries_dir, has_ours })
}

/// Write `files` onto the (writable) shared ESP window and verify them by
/// reading back from the medium. Existing directories are reused; no other
/// file is touched.
pub fn write_shared(dev: &Dev, files: &[(String, Vec<u8>)]) -> io::Result<()> {
    {
        let fs = fatfs::FileSystem::new(dev.handle(), fatfs::FsOptions::new())?;
        for (p, data) in files {
            if let Some((dir, _)) = p.rsplit_once('/') {
                let mut acc = String::new();
                for comp in dir.split('/') {
                    acc = norm_join(&acc, comp);
                    fs.root_dir().create_dir(&acc)?;
                }
            }
            let mut f = fs.root_dir().create_file(p)?;
            f.truncate()?;
            f.write_all(data)?;
            f.flush()?;
        }
        fs.unmount()?;
    }
    dev.sync()?;
    dev.invalidate()?;
    let fs = fatfs::FileSystem::new(dev.handle(), fatfs::FsOptions::new())?;
    for (p, data) in files {
        let mut f = fs.root_dir().open_file(p)?;
        let mut v = Vec::new();
        f.read_to_end(&mut v)?;
        if &v != data {
            return err(format!("shared ESP read-back mismatch for {}", p));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite() {
        let t = "timeout: 0\n\n/LeandrOS\n\tprotocol: limine\n\tpath: boot():/kernel.elf\n\tcmdline: root=PARTUUID=aaaa-02 usbms.delay_us=5 quiet leandros.kid=1\n\n/Two\n\tpath: x\n";
        let r = rewrite_conf(t, "BBBB", true);
        assert!(r.contains("cmdline: root=PARTUUID=BBBB quiet leandros.kid=1 nvme.rw=1"), "{r}");
        assert!(!r.contains("usbms"));
        assert!(r.contains("\tcmdline: root=PARTUUID=BBBB nvme.rw=1\n"), "{r}");
    }
}
