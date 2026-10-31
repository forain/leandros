//! Progress and result output: human text, or one JSON object per line
//! (`--json`). The JSON event set is the GUI contract, see README.md.

use std::io::Write;

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

#[derive(Clone, Copy)]
pub struct Out {
    pub json: bool,
}

impl Out {
    fn line(&self, s: &str) {
        let so = std::io::stdout();
        let mut l = so.lock();
        let _ = writeln!(l, "{}", s);
        let _ = l.flush();
    }

    /// `{"ev":"step","name":N,"pct":P}` (human: "[name] P%").
    pub fn step(&self, name: &str, pct: u32) {
        if self.json {
            self.line(&format!("{{\"ev\":\"step\",\"name\":\"{}\",\"pct\":{}}}", esc(name), pct));
        } else {
            self.line(&format!("[{}] {}%", name, pct));
        }
    }

    /// `{"ev":"info","msg":M}`
    pub fn info(&self, msg: &str) {
        if self.json {
            self.line(&format!("{{\"ev\":\"info\",\"msg\":\"{}\"}}", esc(msg)));
        } else {
            self.line(msg);
        }
    }

    /// Raw pre-built JSON event (json mode) or `human` text.
    pub fn event(&self, json: &str, human: &str) {
        self.line(if self.json { json } else { human });
    }

    /// `{"ev":"error","msg":M}` -- on stdout in json mode, stderr otherwise.
    pub fn error(&self, msg: &str) {
        if self.json {
            self.line(&format!("{{\"ev\":\"error\",\"msg\":\"{}\"}}", esc(msg)));
        } else {
            eprintln!("leandros-install: {}", msg);
        }
    }
}
