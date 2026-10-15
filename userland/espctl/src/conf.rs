//! limine.conf: parsing, and generating the managed (slotted) config.
//!
//! The managed config has up to three entries, in this order:
//!
//!   /LeandrOS trial <id>   the newest deployed kernel, booted once (one-shot)
//!   /LeandrOS good <id>    the last kernel promoted after a successful boot
//!   /LeandrOS factory      the kernel the stick was flashed with (/kernel.elf)
//!
//! Each is the flashed config's first entry with `path:`/`module_path:`
//! pointed at the slot's files and `leandros.slot=<slot> leandros.kid=<id>`
//! appended to the command line, so the running system can tell which entry
//! it was booted from. The flashed config itself is kept verbatim as
//! /leandros/factory.conf and is the template for every regeneration.

pub const MANAGED_MARK: &str = "# espctl-managed v1";
pub const DEFAULT_TIMEOUT: u32 = 3;

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub body: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Conf {
    pub globals: Vec<String>,
    pub entries: Vec<Entry>,
}

fn key_of(line: &str) -> Option<(String, String)> {
    let t = line.trim();
    if t.starts_with('#') {
        return None;
    }
    let (k, v) = t.split_once(':')?;
    Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
}

pub fn parse(text: &str) -> Conf {
    let mut globals = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('/') {
            // Sub-entries ("//name") stay part of the body of the top entry.
            if line.starts_with("//") {
                if let Some(e) = entries.last_mut() {
                    e.body.push(line.to_string());
                }
                continue;
            }
            entries.push(Entry { name: line[1..].trim().to_string(), body: Vec::new() });
        } else if let Some(e) = entries.last_mut() {
            e.body.push(line.to_string());
        } else {
            globals.push(line.to_string());
        }
    }
    for e in &mut entries {
        while e.body.last().map_or(false, |l| l.trim().is_empty()) {
            e.body.pop();
        }
    }
    Conf { globals, entries }
}

pub fn global(c: &Conf, key: &str) -> Option<String> {
    c.globals.iter().filter_map(|l| key_of(l)).find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Slot state read back from a managed config.
#[derive(Clone, Debug, Default)]
pub struct Slots {
    pub trial: Option<String>,
    pub good: Option<String>,
    /// Kernel arguments a slot adds to the flashed entry's command line.
    pub trial_extra: String,
    pub good_extra: String,
    /// 1-based default entry.
    pub default_idx: usize,
    pub timeout: u32,
}

impl Slots {
    pub fn entry_names(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.trial.is_some() {
            v.push("trial");
        }
        if self.good.is_some() {
            v.push("good");
        }
        v.push("factory");
        v
    }
    pub fn default_slot(&self) -> &'static str {
        let n = self.entry_names();
        n.get(self.default_idx.saturating_sub(1)).copied().unwrap_or("factory")
    }
    pub fn set_default(&mut self, slot: &str) {
        let n = self.entry_names();
        self.default_idx = n.iter().position(|s| *s == slot).map(|i| i + 1).unwrap_or(n.len());
    }
    /// The fallback a one-shot trial reverts to.
    pub fn fallback(&self) -> &'static str {
        if self.good.is_some() { "good" } else { "factory" }
    }
}

pub fn is_managed(text: &str) -> bool {
    text.lines().any(|l| l.trim() == MANAGED_MARK)
}

fn entry_cmdline(e: &Entry) -> String {
    e.body.iter().find_map(|l| match key_of(l) {
        Some((k, v)) if k == "cmdline" || k == "kernel_cmdline" => Some(v),
        _ => None,
    }).unwrap_or_default()
}

/// The arguments of `e`'s command line beyond the factory entry's (and the
/// slot tags espctl adds).
fn extra_of(e: &Entry, factory: &Conf) -> String {
    let base: Vec<String> = factory.entries.first().map(entry_cmdline).unwrap_or_default()
        .split_whitespace().map(|t| t.to_string()).collect();
    let mut toks: Vec<String> = entry_cmdline(e).split_whitespace()
        .filter(|t| !t.starts_with("leandros.slot=") && !t.starts_with("leandros.kid="))
        .map(|t| t.to_string()).collect();
    for b in &base {
        if let Some(i) = toks.iter().position(|t| t == b) {
            toks.remove(i);
        }
    }
    toks.join(" ")
}

pub fn read_slots(text: &str, factory: &Conf) -> Slots {
    let c = parse(text);
    let mut s = Slots { timeout: DEFAULT_TIMEOUT, default_idx: 1, ..Default::default() };
    if !is_managed(text) {
        return s;
    }
    for e in &c.entries {
        let mut w = e.name.split_whitespace();
        let (a, b, id) = (w.next(), w.next(), w.next());
        if a == Some("LeandrOS") {
            match (b, id) {
                (Some("trial"), Some(id)) => {
                    s.trial = Some(id.to_string());
                    s.trial_extra = extra_of(e, factory);
                }
                (Some("good"), Some(id)) => {
                    s.good = Some(id.to_string());
                    s.good_extra = extra_of(e, factory);
                }
                _ => {}
            }
        }
    }
    if let Some(t) = global(&c, "timeout").and_then(|v| v.parse().ok()) {
        s.timeout = t;
    }
    if let Some(d) = global(&c, "default_entry").and_then(|v| v.parse().ok()) {
        s.default_idx = d;
    }
    s
}

pub fn slot_dir(id: &str) -> String {
    format!("/leandros/k/{}", id)
}

fn with_slot(base: &Entry, name: String, slot: &str, id: Option<&str>, extra: &str) -> Entry {
    let mut tag = match id {
        Some(id) => format!("leandros.slot={} leandros.kid={}", slot, id),
        None => format!("leandros.slot={}", slot),
    };
    if !extra.trim().is_empty() {
        tag = format!("{} {}", extra.trim(), tag);
    }
    let indent = base.body.iter().find(|l| !l.trim().is_empty()).map(|l| {
        let n = l.len() - l.trim_start().len();
        l[..n].to_string()
    }).unwrap_or_else(|| "\t".into());
    let mut body = Vec::new();
    let mut have_cmdline = false;
    let mut have_module = false;
    for l in &base.body {
        match key_of(l) {
            Some((k, v)) if k == "path" || k == "kernel_path" => {
                match id {
                    Some(id) => body.push(format!("{}{}: boot():{}/kernel.elf", indent, k, slot_dir(id))),
                    None => body.push(l.clone()),
                }
                let _ = v;
            }
            Some((k, _)) if k == "module_path" && !have_module => {
                have_module = true;
                match id {
                    Some(id) => body.push(format!("{}module_path: boot():{}/initrd.gz", indent, slot_dir(id))),
                    None => body.push(l.clone()),
                }
            }
            Some((k, v)) if k == "cmdline" || k == "kernel_cmdline" => {
                have_cmdline = true;
                let v = v.split_whitespace().filter(|t| !t.starts_with("leandros.slot=") && !t.starts_with("leandros.kid=")).collect::<Vec<_>>().join(" ");
                body.push(if v.is_empty() { format!("{}{}: {}", indent, k, tag) } else { format!("{}{}: {} {}", indent, k, v, tag) });
            }
            _ => body.push(l.clone()),
        }
    }
    if !have_cmdline {
        // Right after the path line.
        let at = body.iter().position(|l| matches!(key_of(l), Some((k, _)) if k == "path" || k == "kernel_path")).map(|i| i + 1).unwrap_or(body.len());
        body.insert(at, format!("{}cmdline: {}", indent, tag));
    }
    if !have_module {
        if let Some(id) = id {
            body.push(format!("{}module_path: boot():{}/initrd.gz", indent, slot_dir(id)));
            body.push(format!("{}module_cmdline: initrd", indent));
        }
    }
    Entry { name, body }
}

/// The managed config for `slots`, built from the flashed config `factory`.
pub fn generate(factory: &Conf, slots: &Slots) -> Result<String, String> {
    let base = factory.entries.first().ok_or("the factory limine.conf has no entry")?;
    let mut out = String::new();
    out.push_str(MANAGED_MARK);
    out.push('\n');
    out.push_str("# Generated by /usr/sbin/espctl (kernel slots: trial = one-shot, good = promoted,\n");
    out.push_str("# factory = as flashed). The flashed original is /leandros/factory.conf.\n");
    out.push_str(&format!("timeout: {}\n", slots.timeout));
    out.push_str(&format!("default_entry: {}\n", slots.default_idx));
    for g in &factory.globals {
        match key_of(g) {
            Some((k, _)) if k == "timeout" || k == "default_entry" || k == "remember_last_entry" => {}
            _ => {
                if !(g.trim().is_empty() && out.ends_with("\n\n")) {
                    out.push_str(g);
                    out.push('\n');
                }
            }
        }
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
    let mut entries = Vec::new();
    if let Some(id) = &slots.trial {
        entries.push(with_slot(base, format!("LeandrOS trial {}", id), "trial", Some(id), &slots.trial_extra));
    }
    if let Some(id) = &slots.good {
        entries.push(with_slot(base, format!("LeandrOS good {}", id), "good", Some(id), &slots.good_extra));
    }
    entries.push(with_slot(base, "LeandrOS factory".into(), "factory", None, ""));
    for e in entries {
        out.push('/');
        out.push_str(&e.name);
        out.push('\n');
        for l in e.body {
            out.push_str(&l);
            out.push('\n');
        }
        out.push('\n');
    }
    Ok(out)
}

/// The kernel command line's value for `key=` (e.g. "leandros.slot").
pub fn cmdline_value(cmdline: &str, key: &str) -> Option<String> {
    let p = format!("{}=", key);
    cmdline.split_whitespace().find_map(|t| t.strip_prefix(p.as_str()).map(|v| v.to_string()))
}
