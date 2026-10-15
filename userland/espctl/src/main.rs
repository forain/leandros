//! espctl — the EFI system partition from the running system.
//!
//! LeandrOS has no FAT driver in the VFS; the ESP is reached as a raw
//! partition node (`/dev/sda1`) and read and written here with the `fatfs`
//! crate. The main job is crash-safe kernel updates on a stick booted by
//! Limine (see conf.rs for the entry layout):
//!
//!   install   copy a kernel (+ initrd) into a new slot directory, read it back
//!             from the medium and compare SHA-256, then commit a limine.conf
//!             whose default is the new "trial" entry
//!   boot      run by init once per boot: a trial kernel that got this far
//!             flips the default back to the fallback (good, else factory),
//!             so a trial is booted automatically exactly once; also imports
//!             /leandros/authorized_keys from the ESP into root's keys
//!   promote   after a successful trial boot: trial becomes "good"
//!
//! Nothing that an entry references is ever overwritten: slot files live in
//! /leandros/k/<id>/ (id = hash of kernel and initrd), and the only in-place
//! change is limine.conf, committed through a shadow copy at
//! /limine/limine.conf, which Limine's search order prefers over /limine.conf
//! (see `commit_conf`). Every step ends with a sync of the device.

mod conf;
mod dev;
mod disks;
mod klog;

use conf::Slots;
use dev::Dev;
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

type Fs = fatfs::FileSystem<Dev>;

/// Limine's config search order (EFI app directory first, then the
/// per-partition list), as fatfs paths.
const CONF_SEARCH: &[&str] = &["EFI/BOOT/limine.conf", "boot/limine/limine.conf", "boot/limine.conf", "limine/limine.conf", "limine.conf"];
const PRIMARY: &str = "limine.conf";
const SHADOW_DIR: &str = "limine";
const SHADOW: &str = "limine/limine.conf";
const SHADOW_TMP: &str = "limine/limine.tmp";
const FACTORY_CONF: &str = "leandros/factory.conf";
const SLOT_ROOT: &str = "leandros/k";
const ESP_KEYS: &str = "leandros/authorized_keys";

fn err<T>(msg: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Other, msg.into()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn sha256(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn norm(p: &str) -> &str {
    p.trim_start_matches('/')
}

fn cmdline() -> String {
    std::fs::read_to_string("/proc/cmdline").unwrap_or_default()
}

// ── The ESP ──────────────────────────────────────────────────────────────────

/// Test hook: ESPCTL_FAULT_STEP=<n> exits (status 99) right after the n-th
/// synced step, as a power cut there would leave the medium. Used by
/// host-side tests (espctl --dev <FAT image>) to check that every prefix of
/// a commit leaves a bootable config.
fn fault_point() {
    use std::sync::atomic::{AtomicU32, Ordering};
    static STEPS: AtomicU32 = AtomicU32::new(0);
    let n = STEPS.fetch_add(1, Ordering::Relaxed) + 1;
    if std::env::var("ESPCTL_FAULT_STEP").ok().and_then(|v| v.parse::<u32>().ok()) == Some(n) {
        eprintln!("espctl: ESPCTL_FAULT_STEP={}: stopping here", n);
        std::process::exit(99);
    }
}

struct Esp {
    path: String,
    dev: Dev,
}

impl Esp {
    fn open(path: &str, writable: bool) -> io::Result<Esp> {
        let dev = Dev::open(path, writable).map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path, e)))?;
        Ok(Esp { path: path.to_string(), dev })
    }

    fn fs(&self) -> io::Result<Fs> {
        fatfs::FileSystem::new(self.dev.handle(), fatfs::FsOptions::new())
    }

    /// One ordered step: mount, run `f`, unmount, write everything out and
    /// fsync. A crash leaves the medium either before or after this step's
    /// blocks only in the sense the callers arrange (see commit_conf).
    fn step<R>(&self, f: impl FnOnce(&Fs) -> io::Result<R>) -> io::Result<R> {
        let fs = self.fs()?;
        let r = f(&fs)?;
        fs.unmount()?;
        self.dev.sync()?;
        fault_point();
        Ok(r)
    }

    /// Read-only access; nothing is written.
    fn read<R>(&self, f: impl FnOnce(&Fs) -> io::Result<R>) -> io::Result<R> {
        let fs = self.fs()?;
        f(&fs)
    }
}

fn exists(fs: &Fs, path: &str) -> bool {
    let p = norm(path);
    if p.is_empty() {
        return true;
    }
    fs.root_dir().open_file(p).is_ok() || fs.root_dir().open_dir(p).is_ok()
}

fn read_file(fs: &Fs, path: &str) -> io::Result<Vec<u8>> {
    let mut f = fs.root_dir().open_file(norm(path)).map_err(|e| io::Error::new(e.kind(), format!("ESP {}: {}", path, e)))?;
    let mut v = Vec::new();
    f.read_to_end(&mut v)?;
    Ok(v)
}

fn mkdirs(fs: &Fs, dir: &str) -> io::Result<()> {
    let mut cur = String::new();
    for comp in norm(dir).split('/').filter(|c| !c.is_empty()) {
        if !cur.is_empty() {
            cur.push('/');
        }
        cur.push_str(comp);
        fs.root_dir().create_dir(&cur)?;
    }
    Ok(())
}

fn write_file(fs: &Fs, path: &str, data: &[u8]) -> io::Result<()> {
    let p = norm(path);
    if let Some((parent, _)) = p.rsplit_once('/') {
        mkdirs(fs, parent)?;
    }
    let mut f = fs.root_dir().create_file(p)?;
    f.truncate()?;
    for chunk in data.chunks(256 * 1024) {
        f.write_all(chunk)?;
    }
    f.flush()?;
    Ok(())
}

fn remove_tree(fs: &Fs, path: &str) -> io::Result<()> {
    let p = norm(path);
    if let Ok(d) = fs.root_dir().open_dir(p) {
        let names: Vec<(String, bool)> = d.iter().filter_map(|e| e.ok()).map(|e| (e.file_name(), e.is_dir())).filter(|(n, _)| n != "." && n != "..").collect();
        for (n, is_dir) in names {
            let child = format!("{}/{}", p, n);
            if is_dir {
                remove_tree(fs, &child)?;
            } else {
                fs.root_dir().remove(&child)?;
            }
        }
    }
    fs.root_dir().remove(p)
}

fn list_dir(fs: &Fs, path: &str) -> io::Result<Vec<(String, bool, u64)>> {
    let p = norm(path);
    let d = if p.is_empty() { fs.root_dir() } else { fs.root_dir().open_dir(p)? };
    let mut v: Vec<(String, bool, u64)> = d.iter().filter_map(|e| e.ok()).map(|e| (e.file_name(), e.is_dir(), e.len())).filter(|(n, _, _)| n != "." && n != "..").collect();
    v.sort();
    Ok(v)
}

/// (path, text) of the config Limine will use.
fn effective_conf(fs: &Fs) -> Option<(&'static str, String)> {
    for p in CONF_SEARCH {
        if let Ok(f) = fs.root_dir().open_file(p) {
            let mut v = Vec::new();
            let mut f = f;
            if f.read_to_end(&mut v).is_ok() {
                return Some((p, String::from_utf8_lossy(&v).into_owned()));
            }
        }
    }
    None
}

// ── Finding the ESP ──────────────────────────────────────────────────────────

/// The ESP this system was booted from: an ESP partition holding a
/// limine.conf; with several, the one whose config carries this boot's
/// `root=` (or `leandros.kid=`) value. `--dev`/ESPCTL_DEV override.
fn find_esp(explicit: Option<&str>) -> io::Result<String> {
    if let Some(d) = explicit {
        return Ok(d.to_string());
    }
    if let Ok(d) = std::env::var("ESPCTL_DEV") {
        if !d.is_empty() {
            return Ok(d);
        }
    }
    let cl = cmdline();
    let root = conf::cmdline_value(&cl, "root");
    let kid = conf::cmdline_value(&cl, "leandros.kid");
    let mut cands: Vec<(String, u32)> = Vec::new();
    for p in disks::scan_all().into_iter().filter(|p| p.kind == disks::Kind::Esp) {
        let esp = match Esp::open(&p.dev, false) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let text = match esp.read(|fs| Ok(effective_conf(fs))) {
            Ok(Some((_, t))) => t,
            _ => continue,
        };
        let mut score = 1;
        if let Some(r) = &root {
            if text.contains(&format!("root={}", r)) {
                score += 2;
            }
        }
        if let Some(k) = &kid {
            if text.contains(&format!("leandros.kid={}", k)) {
                score += 4;
            }
        }
        cands.push((p.dev, score));
    }
    cands.sort_by(|a, b| b.1.cmp(&a.1));
    match cands.len() {
        0 => err("no ESP with a limine.conf found (pass --dev /dev/<partition>)"),
        1 => Ok(cands.remove(0).0),
        _ if cands[0].1 > cands[1].1 => Ok(cands.remove(0).0),
        _ => err(format!(
            "several ESPs with a limine.conf and none matches this boot: {} (pass --dev)",
            cands.iter().map(|c| c.0.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

// ── The managed config ───────────────────────────────────────────────────────

struct State {
    conf_path: &'static str,
    text: String,
    factory: conf::Conf,
    factory_saved: bool,
    slots: Slots,
}

fn load_state(esp: &Esp) -> io::Result<State> {
    esp.read(|fs| {
        let (conf_path, text) = match effective_conf(fs) {
            Some(x) => x,
            None => return err("the ESP has no limine.conf"),
        };
        let (factory_text, saved) = match read_file(fs, FACTORY_CONF) {
            Ok(v) => (String::from_utf8_lossy(&v).into_owned(), true),
            Err(_) => {
                if conf::is_managed(&text) {
                    return err("limine.conf is managed but /leandros/factory.conf is missing; re-flash the stick");
                }
                (text.clone(), false)
            }
        };
        let factory = conf::parse(&factory_text);
        let slots = conf::read_slots(&text, &factory);
        Ok(State { conf_path, text, factory, factory_saved: saved, slots })
    })
}

/// Save the flashed config once, before the first managed write.
fn ensure_factory(esp: &Esp, st: &mut State) -> io::Result<()> {
    if st.factory_saved {
        return Ok(());
    }
    if st.factory.entries.is_empty() {
        return err("limine.conf has no boot entry");
    }
    let text = st.text.clone();
    esp.step(|fs| write_file(fs, FACTORY_CONF, text.as_bytes()))?;
    st.factory_saved = true;
    Ok(())
}

/// Replace limine.conf so that at every instant Limine finds a complete
/// config, old or new:
///   0. a shadow left by an interrupted commit is folded into the primary
///   1. write the new text to /limine/limine.tmp
///   2. rename it to /limine/limine.conf: the shadow, found before /limine.conf
///   3. rewrite /limine.conf (the shadow covers a torn write)
///   4. remove the shadow
/// with a device sync after each step.
fn commit_conf(esp: &Esp, st: &State, text: &str) -> io::Result<()> {
    if st.conf_path != PRIMARY && st.conf_path != SHADOW {
        return err(format!("Limine uses /{}; espctl only manages /limine.conf layouts", st.conf_path));
    }
    let shadow_left = esp.read(|fs| Ok(exists(fs, SHADOW)))?;
    if shadow_left {
        let old = esp.read(|fs| read_file(fs, SHADOW))?;
        esp.step(|fs| write_file(fs, PRIMARY, &old))?;
        esp.step(|fs| fs.root_dir().remove(SHADOW))?;
    }
    esp.step(|fs| {
        mkdirs(fs, SHADOW_DIR)?;
        if exists(fs, SHADOW_TMP) {
            fs.root_dir().remove(SHADOW_TMP)?;
        }
        write_file(fs, SHADOW_TMP, text.as_bytes())
    })?;
    esp.step(|fs| {
        let d = fs.root_dir().open_dir(SHADOW_DIR)?;
        d.rename("limine.tmp", &d, "limine.conf")
    })?;
    esp.step(|fs| write_file(fs, PRIMARY, text.as_bytes()))?;
    esp.step(|fs| fs.root_dir().remove(SHADOW))?;
    // Read back what Limine will see.
    esp.dev.invalidate()?;
    let back = esp.read(|fs| Ok(effective_conf(fs)))?;
    match back {
        Some((p, t)) if p == PRIMARY && t == text => Ok(()),
        Some((p, _)) => err(format!("limine.conf read-back mismatch (Limine would use /{})", p)),
        None => err("limine.conf vanished after the commit"),
    }
}

fn write_slots(esp: &Esp, st: &mut State, slots: &Slots) -> io::Result<()> {
    ensure_factory(esp, st)?;
    let text = conf::generate(&st.factory, slots).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    commit_conf(esp, st, &text)?;
    st.text = text;
    st.conf_path = PRIMARY;
    st.slots = slots.clone();
    Ok(())
}

/// Remove slot directories no entry references.
fn gc(esp: &Esp, slots: &Slots) -> io::Result<Vec<String>> {
    let keep: Vec<&String> = slots.trial.iter().chain(slots.good.iter()).collect();
    let names = esp.read(|fs| Ok(list_dir(fs, SLOT_ROOT).unwrap_or_default()))?;
    let mut removed = Vec::new();
    for (n, is_dir, _) in names {
        if is_dir && !keep.iter().any(|k| **k == n) {
            removed.push(n);
        }
    }
    if !removed.is_empty() {
        esp.step(|fs| {
            for n in &removed {
                remove_tree(fs, &format!("{}/{}", SLOT_ROOT, n))?;
            }
            Ok(())
        })?;
    }
    Ok(removed)
}

/// The initrd a slot without its own initrd falls back to: the fallback
/// entry's module.
fn fallback_initrd(esp: &Esp, st: &State) -> io::Result<Vec<u8>> {
    if let Some(g) = &st.slots.good {
        return esp.read(|fs| read_file(fs, &format!("{}/initrd.gz", conf::slot_dir(g))));
    }
    let base = st.factory.entries.first().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no factory entry"))?;
    let mp = base.body.iter().find_map(|l| {
        let t = l.trim();
        let (k, v) = t.split_once(':')?;
        if k.trim().eq_ignore_ascii_case("module_path") { Some(v.trim().to_string()) } else { None }
    }).ok_or_else(|| io::Error::new(io::ErrorKind::Other, "the factory entry has no module_path; pass --initrd"))?;
    let p = mp.strip_prefix("boot():").ok_or_else(|| io::Error::new(io::ErrorKind::Other, format!("factory module_path {} is not boot():; pass --initrd", mp)))?;
    esp.read(|fs| read_file(fs, p))
}

// ── Commands ─────────────────────────────────────────────────────────────────

struct Opts {
    dev: Option<String>,
    args: Vec<String>,
}

fn take_opt(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    if i + 1 >= args.len() {
        eprintln!("espctl: {} needs a value", name);
        std::process::exit(2);
    }
    let v = args.remove(i + 1);
    args.remove(i);
    Some(v)
}

fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    match args.iter().position(|a| a == name) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    }
}

fn open_esp(o: &Opts, writable: bool) -> io::Result<Esp> {
    let p = find_esp(o.dev.as_deref())?;
    Esp::open(&p, writable)
}

fn booted() -> (Option<String>, Option<String>) {
    let cl = cmdline();
    (conf::cmdline_value(&cl, "leandros.slot"), conf::cmdline_value(&cl, "leandros.kid"))
}

fn print_status(esp: &Esp, st: &State) -> io::Result<()> {
    let (slot, kid) = booted();
    println!("esp={}", esp.path);
    println!("config=/{}", st.conf_path);
    println!("managed={}", if conf::is_managed(&st.text) { 1 } else { 0 });
    println!("booted_slot={}", slot.as_deref().unwrap_or("unmanaged"));
    println!("booted_kid={}", kid.as_deref().unwrap_or("-"));
    println!("trial={}", st.slots.trial.as_deref().unwrap_or("-"));
    println!("trial_cmdline={}", st.slots.trial_extra);
    println!("good={}", st.slots.good.as_deref().unwrap_or("-"));
    println!("good_cmdline={}", st.slots.good_extra);
    println!("default={}", if conf::is_managed(&st.text) { st.slots.default_slot() } else { "factory" });
    println!("armed={}", if conf::is_managed(&st.text) && st.slots.default_slot() == "trial" { 1 } else { 0 });
    println!("timeout={}", conf::global(&conf::parse(&st.text), "timeout").unwrap_or_else(|| "?".into()));
    let (free, total) = esp.read(|fs| {
        let s = fs.stats()?;
        Ok((s.free_clusters() as u64 * s.cluster_size() as u64, s.total_clusters() as u64 * s.cluster_size() as u64))
    })?;
    println!("esp_free_bytes={}", free);
    println!("esp_total_bytes={}", total);
    for (name, id) in [("trial", &st.slots.trial), ("good", &st.slots.good)] {
        if let Some(id) = id {
            for f in ["kernel.elf", "initrd.gz"] {
                let p = format!("{}/{}", conf::slot_dir(id), f);
                match esp.read(|fs| read_file(fs, &p)) {
                    Ok(v) => println!("{}_{}_sha256={} size={}", name, f.split('.').next().unwrap(), sha256(&v), v.len()),
                    Err(e) => println!("{}_{}_sha256=MISSING ({})", name, f.split('.').next().unwrap(), e),
                }
            }
        }
    }
    Ok(())
}

fn cmd_status(o: &Opts) -> io::Result<()> {
    let esp = open_esp(o, false)?;
    let st = load_state(&esp)?;
    print_status(&esp, &st)
}

fn read_local(path: &str) -> io::Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path, e)))
}

fn cmd_install(o: &mut Opts) -> io::Result<()> {
    let initrd_path = take_opt(&mut o.args, "--initrd");
    let want_k = take_opt(&mut o.args, "--sha256");
    let want_i = take_opt(&mut o.args, "--initrd-sha256");
    let no_arm = take_flag(&mut o.args, "--no-arm");
    let extra_arg = take_opt(&mut o.args, "--cmdline");
    if o.args.len() != 1 {
        return err("usage: espctl install <kernel> [--initrd <file>] [--sha256 <hex>] [--initrd-sha256 <hex>] [--cmdline <args>] [--no-arm]");
    }
    let kernel = read_local(&o.args[0])?;
    if kernel.len() < 64 || &kernel[0..4] != b"\x7fELF" {
        return err(format!("{} is not an ELF file", o.args[0]));
    }
    let ksha = sha256(&kernel);
    if let Some(w) = &want_k {
        if !w.eq_ignore_ascii_case(&ksha) {
            return err(format!("kernel sha256 {} does not match the expected {}", ksha, w));
        }
    }
    let esp = open_esp(o, true)?;
    let mut st = load_state(&esp)?;
    let initrd = match &initrd_path {
        Some(p) => read_local(p)?,
        None => fallback_initrd(&esp, &st)?,
    };
    let isha = sha256(&initrd);
    if let Some(w) = &want_i {
        if !w.eq_ignore_ascii_case(&isha) {
            return err(format!("initrd sha256 {} does not match the expected {}", isha, w));
        }
    }
    let id = sha256(format!("{}\n{}\n", ksha, isha).as_bytes())[..12].to_string();
    println!("kernel sha256={} ({} bytes)", ksha, kernel.len());
    println!("initrd sha256={} ({} bytes){}", isha, initrd.len(), if initrd_path.is_none() { " [reused]" } else { "" });
    println!("slot id={}", id);
    // Extra kernel arguments: --cmdline, else those of the good slot.
    let extra = extra_arg.unwrap_or_else(|| st.slots.good_extra.clone()).split_whitespace().collect::<Vec<_>>().join(" ");
    if extra.contains("leandros.slot=") || extra.contains("leandros.kid=") {
        return err("--cmdline must not set leandros.slot/leandros.kid");
    }
    println!("extra cmdline: {}", if extra.is_empty() { "(none)" } else { extra.as_str() });
    if st.slots.good.as_deref() == Some(id.as_str()) && st.slots.good_extra == extra {
        println!("this kernel, with these arguments, is already the good slot; nothing to do");
        return Ok(());
    }
    ensure_factory(&esp, &mut st)?;
    // Unreferenced leftovers first, then make room if the old trial is in the way.
    gc(&esp, &st.slots)?;
    let need = (kernel.len() + initrd.len()) as u64 + (1 << 20);
    let free = esp.read(|fs| {
        let s = fs.stats()?;
        Ok(s.free_clusters() as u64 * s.cluster_size() as u64)
    })?;
    if free < need {
        if st.slots.trial.is_some() && st.slots.trial.as_deref() != Some(id.as_str()) {
            println!("ESP: {} bytes free, {} needed: dropping the old trial slot first", free, need);
            let mut s = st.slots.clone();
            let d = if s.default_slot() == "trial" { s.fallback() } else { s.default_slot() };
            s.trial = None;
            s.set_default(d);
            write_slots(&esp, &mut st, &s)?;
            gc(&esp, &st.slots)?;
        } else {
            return err(format!("ESP: {} bytes free, {} needed", free, need));
        }
    }
    let dir = conf::slot_dir(&id);
    let t0 = std::time::Instant::now();
    esp.step(|fs| {
        write_file(fs, &format!("{}/kernel.elf", dir), &kernel)?;
        write_file(fs, &format!("{}/initrd.gz", dir), &initrd)
    })?;
    // Verify from the medium, not the cache.
    esp.dev.invalidate()?;
    let (k2, i2) = esp.read(|fs| Ok((read_file(fs, &format!("{}/kernel.elf", dir))?, read_file(fs, &format!("{}/initrd.gz", dir))?)))?;
    if sha256(&k2) != ksha || sha256(&i2) != isha {
        let _ = esp.step(|fs| remove_tree(fs, &dir));
        return err("read-back verification FAILED: the slot was removed, limine.conf is unchanged");
    }
    println!("wrote and verified {} in {:.1} s", dir, t0.elapsed().as_secs_f64());
    let mut s = st.slots.clone();
    let d = s.default_slot();
    s.trial = Some(id.clone());
    s.trial_extra = extra.clone();
    if no_arm { s.set_default(if d == "trial" { s.fallback() } else { d }) } else { s.set_default("trial") }
    write_slots(&esp, &mut st, &s)?;
    let removed = gc(&esp, &st.slots)?;
    for r in removed {
        println!("removed old slot {}", r);
    }
    println!("{}", if no_arm { "installed as trial (not armed: `espctl arm` makes it the next boot)" } else { "armed: the next boot runs the trial kernel once; later boots fall back unless it is promoted" });
    print_status(&esp, &st)
}

fn modify(o: &Opts, f: impl FnOnce(&mut Slots, &State) -> io::Result<Option<String>>) -> io::Result<()> {
    let esp = open_esp(o, true)?;
    let mut st = load_state(&esp)?;
    let mut s = st.slots.clone();
    match f(&mut s, &st)? {
        Some(msg) => {
            write_slots(&esp, &mut st, &s)?;
            for r in gc(&esp, &st.slots)? {
                println!("removed old slot {}", r);
            }
            println!("{}", msg);
        }
        None => {}
    }
    print_status(&esp, &st)
}

fn cmd_arm(o: &Opts) -> io::Result<()> {
    modify(o, |s, _| {
        if s.trial.is_none() {
            return err("no trial kernel installed");
        }
        if s.default_slot() == "trial" {
            println!("already armed");
            return Ok(None);
        }
        s.set_default("trial");
        Ok(Some("armed: the next boot runs the trial kernel".into()))
    })
}

fn cmd_disarm(o: &Opts) -> io::Result<()> {
    modify(o, |s, _| {
        if s.default_slot() != "trial" {
            println!("not armed");
            return Ok(None);
        }
        let fb = s.fallback();
        s.set_default(fb);
        Ok(Some(format!("disarmed: the next boot runs the {} kernel", fb)))
    })
}

fn cmd_promote(o: &mut Opts) -> io::Result<()> {
    let force = take_flag(&mut o.args, "--force");
    modify(o, |s, _| {
        let trial = match &s.trial {
            Some(t) => t.clone(),
            None => return err("no trial kernel to promote"),
        };
        let (slot, kid) = booted();
        if !force && (slot.as_deref() != Some("trial") || kid.as_deref() != Some(trial.as_str())) {
            return err(format!(
                "the running kernel is {} ({}), not the trial {}: boot it first, or pass --force",
                slot.as_deref().unwrap_or("unmanaged"), kid.as_deref().unwrap_or("-"), trial
            ));
        }
        s.good = Some(trial.clone());
        s.good_extra = s.trial_extra.clone();
        s.trial = None;
        s.trial_extra.clear();
        s.set_default("good");
        Ok(Some(format!("promoted {} to good; it is the default from now on", trial)))
    })
}

fn cmd_discard(o: &Opts) -> io::Result<()> {
    modify(o, |s, _| {
        if s.trial.is_none() {
            println!("no trial kernel");
            return Ok(None);
        }
        let d = s.default_slot();
        s.trial = None;
        s.trial_extra.clear();
        let fb = s.fallback();
        s.set_default(if d == "trial" { fb } else { d });
        Ok(Some("trial slot removed".into()))
    })
}

fn cmd_default(o: &Opts) -> io::Result<()> {
    let want = o.args.get(0).cloned().unwrap_or_default();
    modify(o, |s, _| {
        if !s.entry_names().contains(&want.as_str()) {
            return err(format!("no {} entry (entries: {})", want, s.entry_names().join(", ")));
        }
        s.set_default(&want);
        Ok(Some(format!("default entry: {}", want)))
    })
}

/// Set the extra kernel arguments of an existing trial or good entry.
fn cmd_cmdline(o: &Opts) -> io::Result<()> {
    let slot = o.args.get(0).cloned().unwrap_or_default();
    let extra = o.args[1.min(o.args.len())..].join(" ");
    if extra.contains("leandros.slot=") || extra.contains("leandros.kid=") {
        return err("must not set leandros.slot/leandros.kid");
    }
    modify(o, |s, _| {
        match slot.as_str() {
            "trial" if s.trial.is_some() => s.trial_extra = extra.clone(),
            "good" if s.good.is_some() => s.good_extra = extra.clone(),
            _ => return err("usage: espctl cmdline <trial|good> [args...] (the slot must exist)"),
        }
        Ok(Some(format!("{} entry: extra cmdline now '{}'", slot, extra)))
    })
}

fn cmd_timeout(o: &Opts) -> io::Result<()> {
    let t: u32 = o.args.get(0).and_then(|v| v.parse().ok()).ok_or_else(|| io::Error::new(io::ErrorKind::Other, "usage: espctl timeout <seconds>"))?;
    modify(o, |s, _| {
        s.timeout = t;
        Ok(Some(format!("menu timeout: {} s", t)))
    })
}

/// Put the flashed limine.conf back and remove every slot.
fn cmd_reset(o: &Opts) -> io::Result<()> {
    let esp = open_esp(o, true)?;
    let st = load_state(&esp)?;
    if !st.factory_saved {
        println!("limine.conf is the flashed one; nothing to reset");
        return Ok(());
    }
    let text = esp.read(|fs| read_file(fs, FACTORY_CONF))?;
    commit_conf(&esp, &st, &String::from_utf8_lossy(&text))?;
    let empty = Slots::default();
    gc(&esp, &empty)?;
    println!("limine.conf restored from /leandros/factory.conf; slots removed");
    Ok(())
}

/// Merge /leandros/authorized_keys from the ESP into root's keys.
fn import_keys(esp: &Esp) -> io::Result<usize> {
    let keys = match esp.read(|fs| Ok(if exists(fs, ESP_KEYS) { Some(read_file(fs, ESP_KEYS)?) } else { None }))? {
        Some(k) => String::from_utf8_lossy(&k).into_owned(),
        None => return Ok(0),
    };
    let dir = "/root/.ssh";
    let file = "/root/.ssh/authorized_keys";
    if std::fs::metadata(dir).is_err() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let existing = std::fs::read_to_string(file).unwrap_or_default();
    let have: Vec<&str> = existing.lines().map(|l| l.trim()).collect();
    let mut add = Vec::new();
    for l in keys.lines().map(|l| l.trim()) {
        if l.is_empty() || l.starts_with('#') || have.contains(&l) || add.contains(&l) {
            continue;
        }
        add.push(l);
    }
    if add.is_empty() {
        return Ok(0);
    }
    let mut out = existing.clone();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for l in &add {
        out.push_str(l);
        out.push('\n');
    }
    let tmp = "/root/.ssh/authorized_keys.espctl";
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(tmp)?;
        f.write_all(out.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, file)?;
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))?;
    Ok(add.len())
}

fn cmd_import_keys(o: &Opts) -> io::Result<()> {
    let esp = open_esp(o, false)?;
    let n = import_keys(&esp)?;
    println!("imported {} key(s) from the ESP into /root/.ssh/authorized_keys", n);
    Ok(())
}

/// init's once-per-boot hook. Quiet when there is nothing to do.
fn cmd_boot(o: &Opts) -> io::Result<()> {
    let esp = match open_esp(o, true) {
        Ok(e) => e,
        Err(e) => {
            println!("espctl: no boot ESP ({})", e);
            return Ok(());
        }
    };
    let (slot, kid) = booted();
    let mut st = load_state(&esp)?;
    if conf::is_managed(&st.text) || slot.is_some() {
        println!(
            "espctl: booted {} kernel {} from {} (trial={} good={} default={})",
            slot.as_deref().unwrap_or("unmanaged"), kid.as_deref().unwrap_or("-"), esp.path,
            st.slots.trial.as_deref().unwrap_or("-"), st.slots.good.as_deref().unwrap_or("-"), st.slots.default_slot()
        );
    }
    if conf::is_managed(&st.text) && st.slots.default_slot() == "trial" {
        // The trial was the default. Booted it: one-shot, fall back next time.
        // Booted something else: the trial was skipped in the menu (it failed
        // before init, or was refused), so it must not be the default again.
        let mut s = st.slots.clone();
        let fb = s.fallback();
        s.set_default(fb);
        write_slots(&esp, &mut st, &s)?;
        if slot.as_deref() == Some("trial") {
            println!("espctl: one-shot trial: the next boot defaults to the {} kernel again (`espctl promote` keeps this one)", fb);
        } else {
            println!("espctl: the armed trial kernel was NOT booted (picked {} in the menu?): disarmed, next boot: {}", slot.as_deref().unwrap_or("another entry"), fb);
        }
    }
    match import_keys(&esp) {
        Ok(0) => {}
        Ok(n) => println!("espctl: imported {} key(s) from the ESP into /root/.ssh/authorized_keys", n),
        Err(e) => println!("espctl: key import failed: {}", e),
    }
    Ok(())
}

fn cmd_parts(o: &Opts) -> io::Result<()> {
    let sel = find_esp(o.dev.as_deref()).ok();
    for p in disks::scan_all() {
        println!(
            "{} kind={} disk={} pno={} start={} size={}{}",
            p.dev, p.kind.name(), p.disk, p.pno, p.start, p.size,
            if sel.as_deref() == Some(p.dev.as_str()) { " boot-esp" } else { "" }
        );
    }
    Ok(())
}

/// The console log partition on the boot ESP's disk.
fn log_part(o: &Opts) -> io::Result<disks::Part> {
    let esp = find_esp(o.dev.as_deref())?;
    let parts = disks::scan_all();
    let disk = parts.iter().find(|p| p.dev == esp).map(|p| p.disk.clone());
    parts
        .into_iter()
        .find(|p| p.kind == disks::Kind::Log && Some(&p.disk) == disk.as_ref())
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, format!("no log partition next to {} (stick written by an old write-usb.sh?)", esp)))
}

/// The persistent console log (kernel + console output) of this and the
/// previous boots: the dmesg of this system. `sync` first for the newest lines.
fn cmd_klog(o: &mut Opts) -> io::Result<()> {
    let all = take_flag(&mut o.args, "--all");
    let list = take_flag(&mut o.args, "--list");
    let want: Option<u32> = take_opt(&mut o.args, "--boot").and_then(|v| v.parse().ok());
    let lp = log_part(o)?;
    let boots = klog::read_all(&lp.dev, lp.size)?;
    if boots.is_empty() {
        return err(format!("{}: no log yet", lp.dev));
    }
    let sel: Vec<&klog::Boot> = if let Some(b) = want {
        boots.iter().filter(|x| x.boot == b).collect()
    } else if all || list {
        boots.iter().collect()
    } else {
        boots.iter().rev().take(1).collect()
    };
    if sel.is_empty() {
        return err("no such boot in the log");
    }
    let mut out = io::stdout();
    for b in sel {
        let banner = format!(
            "===== boot {} (slot {}, {} bytes, flush #{} from {}) {}=====\n",
            b.boot, b.slot, b.len, b.seq, if b.journal { "journal" } else { "header" },
            b.notes().iter().map(|n| format!("[{}] ", n)).collect::<String>()
        );
        if list {
            out.write_all(banner.as_bytes())?;
            continue;
        }
        if all || want.is_some() {
            out.write_all(banner.as_bytes())?;
        } else if !b.notes().is_empty() {
            eprint!("{}", banner);
        }
        out.write_all(&b.text)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

fn cmd_ls(o: &Opts) -> io::Result<()> {
    let esp = open_esp(o, false)?;
    let path = o.args.get(0).cloned().unwrap_or_default();
    let v = esp.read(|fs| list_dir(fs, &path))?;
    for (n, d, len) in v {
        if d { println!("{}/", n) } else { println!("{:>12} {}", len, n) }
    }
    Ok(())
}

fn cmd_cat(o: &Opts) -> io::Result<()> {
    let esp = open_esp(o, false)?;
    let p = o.args.get(0).ok_or_else(|| io::Error::new(io::ErrorKind::Other, "usage: espctl cat <esp-path>"))?;
    let v = esp.read(|fs| read_file(fs, p))?;
    io::stdout().write_all(&v)
}

fn cmd_get(o: &Opts) -> io::Result<()> {
    if o.args.len() != 2 {
        return err("usage: espctl get <esp-path> <local>");
    }
    let esp = open_esp(o, false)?;
    let v = esp.read(|fs| read_file(fs, &o.args[0]))?;
    std::fs::write(&o.args[1], &v)?;
    println!("{} bytes sha256={}", v.len(), sha256(&v));
    Ok(())
}

fn cmd_put(o: &Opts) -> io::Result<()> {
    if o.args.len() != 2 {
        return err("usage: espctl put <local> <esp-path>");
    }
    let target = norm(&o.args[1]).to_string();
    if CONF_SEARCH.contains(&target.as_str()) || target.starts_with(SLOT_ROOT) || target == FACTORY_CONF {
        return err(format!("/{} is managed by espctl; use install/promote/default/timeout", target));
    }
    let data = read_local(&o.args[0])?;
    let esp = open_esp(o, true)?;
    esp.step(|fs| write_file(fs, &target, &data))?;
    esp.dev.invalidate()?;
    let back = esp.read(|fs| read_file(fs, &target))?;
    if back != data {
        return err("read-back mismatch");
    }
    println!("{} bytes sha256={}", data.len(), sha256(&data));
    Ok(())
}

fn cmd_rm(o: &Opts) -> io::Result<()> {
    let target = norm(o.args.get(0).map(|s| s.as_str()).unwrap_or("")).to_string();
    if target.is_empty() || CONF_SEARCH.contains(&target.as_str()) || target.starts_with(SLOT_ROOT) || target == FACTORY_CONF || target.starts_with("EFI") {
        return err("refusing to remove that path");
    }
    let esp = open_esp(o, true)?;
    esp.step(|fs| fs.root_dir().remove(&target))
}

const USAGE: &str = "usage: espctl [--dev /dev/<esp>] <command>
  status                 slots, default entry, what this boot is
  install <kernel> [--initrd F] [--sha256 H] [--initrd-sha256 H] [--cmdline ARGS] [--no-arm]
                         new trial slot (verified read-back), armed as next boot;
                         ARGS are appended to the flashed cmdline (default: the
                         good slot's)
  cmdline <trial|good> [ARGS...]  change a slot's extra kernel arguments
  arm | disarm           make the trial the next boot / not
  promote [--force]      the running trial becomes the good kernel
  discard                drop the trial slot
  default <trial|good|factory>
  timeout <seconds>      Limine menu timeout of the managed config
  reset                  restore the flashed limine.conf, remove all slots
  boot                   init's per-boot hook (one-shot disarm, key import)
  import-keys            ESP /leandros/authorized_keys -> /root/.ssh/authorized_keys
  parts                  partitions of every disk (esp, log, home, ...)
  klog [--all|--list|--boot N]  console log partition: this boot (dmesg), or all 4
  ls [dir] | cat <f> | get <f> <local> | put <local> <f> | rm <f>";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let dev = take_opt(&mut args, "--dev");
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        println!("{}", USAGE);
        return;
    }
    let cmd = args.remove(0);
    // One writer at a time.
    let _lock = std::fs::OpenOptions::new().create(true).write(true).open("/run/espctl.lock").ok().map(|f| {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        f
    });
    let mut o = Opts { dev, args };
    let r = match cmd.as_str() {
        "status" => cmd_status(&o),
        "install" => cmd_install(&mut o),
        "arm" => cmd_arm(&o),
        "disarm" => cmd_disarm(&o),
        "promote" => cmd_promote(&mut o),
        "discard" => cmd_discard(&o),
        "default" => cmd_default(&o),
        "timeout" => cmd_timeout(&o),
        "cmdline" => cmd_cmdline(&o),
        "reset" => cmd_reset(&o),
        "boot" => cmd_boot(&o),
        "import-keys" => cmd_import_keys(&o),
        "parts" => cmd_parts(&o),
        "klog" => cmd_klog(&mut o),
        "ls" => cmd_ls(&o),
        "cat" => cmd_cat(&o),
        "get" => cmd_get(&o),
        "put" => cmd_put(&o),
        "rm" => cmd_rm(&o),
        _ => {
            eprintln!("{}", USAGE);
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("espctl {}: {}", cmd, e);
        std::process::exit(1);
    }
}
