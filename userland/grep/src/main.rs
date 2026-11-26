//! grep: a POSIX grep front end over ripgrep. The image ships `/bin/rg`
//! (built unmodified from the sibling ripgrep checkout) and no grep; this
//! translates grep's arguments and BRE patterns and runs rg. Installed as
//! /bin/grep, /bin/egrep (-E) and /bin/fgrep (-F). Exit status 0/1/2 like grep.

use std::process::{Command, ExitCode};

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub rg_args: Vec<String>,
    /// Messages for operands grep would reject (directory without -r).
    pub errors: Vec<String>,
    pub usage_error: Option<String>,
    /// Operands were given but all were rejected.
    pub nothing_to_search: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Syntax {
    Basic,
    Extended,
    Fixed,
    Perl,
}

/// Translate a POSIX basic regular expression into the ERE dialect rg
/// (Rust regex) understands: `\( \) \| \+ \? \{ \}` become operators, the
/// bare characters become literals. Bracket expressions are copied verbatim.
pub fn bre_to_ere(p: &str) -> String {
    let c: Vec<char> = p.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut at_start = true; // a leading `*` is literal in a BRE
    while i < c.len() {
        let ch = c[i];
        match ch {
            '\\' if i + 1 < c.len() => {
                let n = c[i + 1];
                match n {
                    '(' | ')' | '|' | '+' | '?' | '{' | '}' => out.push(n),
                    _ => {
                        out.push('\\');
                        out.push(n);
                    }
                }
                i += 2;
                at_start = n == '(' || n == '|';
                continue;
            }
            '[' => {
                // copy the whole bracket expression
                let mut j = i + 1;
                if j < c.len() && c[j] == '^' { j += 1; }
                if j < c.len() && c[j] == ']' { j += 1; }
                while j < c.len() && c[j] != ']' {
                    if c[j] == '[' && j + 1 < c.len() && matches!(c[j + 1], ':' | '.' | '=') {
                        let close = c[j + 1];
                        j += 2;
                        while j + 1 < c.len() && !(c[j] == close && c[j + 1] == ']') { j += 1; }
                        j += 2;
                        continue;
                    }
                    j += 1;
                }
                let end = (j + 1).min(c.len());
                out.extend(&c[i..end]);
                i = end;
                at_start = false;
                continue;
            }
            '(' | ')' | '|' | '+' | '?' | '{' | '}' => {
                out.push('\\');
                out.push(ch);
            }
            '*' if at_start => out.push_str("\\*"),
            _ => out.push(ch),
        }
        at_start = ch == '^' && i == 0;
        i += 1;
    }
    out
}

/// Build the rg command line. `is_dir` tells whether an operand is a directory.
pub fn plan(prog: &str, args: &[String], is_dir: &dyn Fn(&str) -> bool) -> Plan {
    let mut p = Plan::default();
    let mut syntax = match prog {
        "egrep" => Syntax::Extended,
        "fgrep" => Syntax::Fixed,
        _ => Syntax::Basic,
    };
    let mut recursive = false;
    let mut patterns: Vec<String> = Vec::new();
    let mut have_pat = false;
    let mut pattern_files: Vec<String> = Vec::new();
    let mut operands: Vec<String> = Vec::new();
    let mut flags: Vec<String> = Vec::new();
    let mut i = 0;
    let mut only_operands = false;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if only_operands || a == "-" || !a.starts_with('-') {
            operands.push(a.clone());
            continue;
        }
        if a == "--" {
            only_operands = true;
            continue;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, val) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let mut take = |i: &mut usize| -> Option<String> {
                if val.is_some() { return val.clone(); }
                if *i < args.len() { *i += 1; Some(args[*i - 1].clone()) } else { None }
            };
            match name {
                "extended-regexp" => syntax = Syntax::Extended,
                "fixed-strings" => syntax = Syntax::Fixed,
                "basic-regexp" => syntax = Syntax::Basic,
                "perl-regexp" => syntax = Syntax::Perl,
                "recursive" | "dereference-recursive" => recursive = true,
                "regexp" => match take(&mut i) { Some(v) => { patterns.push(v); have_pat = true; } None => p.usage_error = Some("option '--regexp' requires an argument".into()) },
                "file" => match take(&mut i) { Some(v) => { pattern_files.push(v); have_pat = true; } None => p.usage_error = Some("option '--file' requires an argument".into()) },
                "include" => if let Some(v) = take(&mut i) { flags.push("-g".into()); flags.push(v); },
                "exclude" => if let Some(v) = take(&mut i) { flags.push("-g".into()); flags.push(format!("!{v}")); },
                "ignore-case" | "invert-match" | "count" | "files-with-matches" | "files-without-match"
                | "line-number" | "no-filename" | "with-filename" | "quiet" | "silent" | "word-regexp"
                | "line-regexp" | "only-matching" | "text" | "max-count" | "after-context"
                | "before-context" | "context" | "color" | "colour" => {
                    let rg = match name {
                        "silent" => "quiet",
                        "colour" => "color",
                        "files-without-match" => "files-without-match",
                        n => n,
                    };
                    match val { Some(v) => flags.push(format!("--{rg}={v}")), None => {
                        if matches!(rg, "max-count" | "after-context" | "before-context" | "context") {
                            if let Some(v) = take(&mut i) { flags.push(format!("--{rg}={v}")); }
                        } else { flags.push(format!("--{rg}")); }
                    } }
                }
                "no-messages" => flags.push("--no-messages".into()),
                "help" => flags.push("--help".into()),
                "version" => flags.push("--version".into()),
                _ => flags.push(a.clone()),
            }
            continue;
        }
        // short option cluster
        let cl: Vec<char> = a[1..].chars().collect();
        let mut k = 0;
        while k < cl.len() {
            let f = cl[k];
            k += 1;
            let rest: String = cl[k..].iter().collect();
            match f {
                'E' => syntax = Syntax::Extended,
                'F' => syntax = Syntax::Fixed,
                'G' => syntax = Syntax::Basic,
                'P' => syntax = Syntax::Perl,
                'r' | 'R' => recursive = true,
                's' => flags.push("--no-messages".into()),
                'y' => flags.push("-i".into()),
                'U' | 'u' => {}
                'i' | 'v' | 'c' | 'l' | 'n' | 'h' | 'H' | 'q' | 'w' | 'x' | 'o' | 'a' => {
                    flags.push(format!("-{f}"));
                }
                'L' => flags.push("--files-without-match".into()),
                'e' | 'f' | 'm' | 'A' | 'B' | 'C' => {
                    let v = if !rest.is_empty() { k = cl.len(); Some(rest) }
                        else if i < args.len() { i += 1; Some(args[i - 1].clone()) } else { None };
                    match (f, v) {
                        ('e', Some(v)) => { patterns.push(v); have_pat = true; }
                        ('f', Some(v)) => { pattern_files.push(v); have_pat = true; }
                        ('m', Some(v)) => { flags.push("-m".into()); flags.push(v); }
                        (o, Some(v)) => { flags.push(format!("-{o}")); flags.push(v); }
                        (o, None) => p.usage_error = Some(format!("option requires an argument -- '{o}'")),
                    }
                }
                d if d.is_ascii_digit() => {
                    // -NUM == -C NUM
                    let mut num = d.to_string();
                    while k < cl.len() && cl[k].is_ascii_digit() { num.push(cl[k]); k += 1; }
                    flags.push("-C".into());
                    flags.push(num);
                }
                other => p.usage_error = Some(format!("invalid option -- '{other}'")),
            }
        }
    }
    if p.usage_error.is_some() { return p; }
    if !have_pat {
        if operands.is_empty() {
            p.usage_error = Some("no pattern".into());
            return p;
        }
        patterns.push(operands.remove(0));
    }
    // rg: no config/ignore files, hidden files searched, grep-shaped output.
    p.rg_args.extend(["--no-config", "--no-ignore", "--hidden", "--no-heading", "--color=never", "-N"].map(String::from));
    match syntax {
        Syntax::Fixed => p.rg_args.push("-F".into()),
        Syntax::Perl => p.rg_args.push("-P".into()),
        _ => {}
    }
    // grep -c prints 0 for files without a match; rg omits them unless told.
    if flags.iter().any(|f| f == "-c" || f.starts_with("--count")) {
        p.rg_args.push("--include-zero".into());
    }
    p.rg_args.extend(flags);
    for pat in patterns {
        // a -e/operand pattern may hold several lines: each is a pattern
        for line in pat.split('\n') {
            let t = if syntax == Syntax::Basic { bre_to_ere(line) } else { line.to_string() };
            p.rg_args.push("-e".into());
            p.rg_args.push(t);
        }
    }
    for f in pattern_files {
        p.rg_args.push("-f".into());
        p.rg_args.push(f);
    }
    if operands.is_empty() {
        if !recursive { p.rg_args.push("-".into()); }
    } else {
        let given = operands.len();
        for o in operands {
            if !recursive && o != "-" && is_dir(&o) {
                p.errors.push(format!("{o}: Is a directory"));
            } else {
                p.rg_args.push(o);
            }
        }
        p.nothing_to_search = p.errors.len() == given;
    }
    p
}

fn main() -> ExitCode {
    let mut argv = std::env::args();
    let argv0 = argv.next().unwrap_or_default();
    let prog = argv0.rsplit('/').next().unwrap_or("grep").to_string();
    let args: Vec<String> = argv.collect();
    let pl = plan(&prog, &args, &|p| std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false));
    if let Some(e) = pl.usage_error {
        eprintln!("{prog}: {e}");
        eprintln!("Usage: {prog} [OPTION]... PATTERNS [FILE]...");
        return ExitCode::from(2);
    }
    for e in &pl.errors {
        eprintln!("{prog}: {e}");
    }
    // Every operand was a rejected directory: nothing left to search (rg
    // would fall back to stdin or the cwd).
    if pl.nothing_to_search {
        return ExitCode::from(2);
    }
    let rg = std::env::var("GREP_RG").unwrap_or_else(|_| "/bin/rg".into());
    match Command::new(&rg).args(&pl.rg_args).status() {
        Ok(st) => {
            let code = st.code().unwrap_or(2) as u8;
            if pl.errors.is_empty() { ExitCode::from(code) } else { ExitCode::from(2) }
        }
        Err(e) => {
            eprintln!("{prog}: cannot run {rg}: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn s(v: &[&str]) -> Vec<String> { v.iter().map(|x| x.to_string()).collect() }
    fn nodir(_: &str) -> bool { false }
    fn tail(p: &Plan) -> Vec<String> { p.rg_args[6..].to_vec() }

    #[test]
    fn bre_operators() {
        assert_eq!(bre_to_ere(r"a\|b"), "a|b");
        assert_eq!(bre_to_ere(r"\(ab\)\+"), "(ab)+");
        assert_eq!(bre_to_ere(r"a\{2,3\}"), "a{2,3}");
        assert_eq!(bre_to_ere("a|b+(c)?"), r"a\|b\+\(c\)\?");
        assert_eq!(bre_to_ere("*a"), r"\*a");
        assert_eq!(bre_to_ere("[a|b]+"), r"[a|b]\+");
        assert_eq!(bre_to_ere("[]|]x"), "[]|]x");
        assert_eq!(bre_to_ere(r"a\.b"), r"a\.b");
    }
    #[test]
    fn stdin_when_no_operand() {
        let p = plan("grep", &s(&["-c", "foo"]), &nodir);
        assert_eq!(tail(&p), s(&["-c", "-e", "foo", "-"]));
    }
    #[test]
    fn count_includes_zero() {
        let p = plan("grep", &s(&["-c", "foo", "f"]), &nodir);
        assert!(p.rg_args.contains(&"--include-zero".to_string()));
        let p = plan("grep", &s(&["foo", "f"]), &nodir);
        assert!(!p.rg_args.contains(&"--include-zero".to_string()));
    }
    #[test]
    fn recursive_has_no_dash_and_no_r() {
        let p = plan("grep", &s(&["-r", "PATH", "/etc"]), &nodir);
        assert_eq!(tail(&p), s(&["-e", "PATH", "/etc"]));
        let p = plan("grep", &s(&["-rn", "PATH"]), &nodir);
        assert_eq!(tail(&p), s(&["-n", "-e", "PATH"]));
    }
    #[test]
    fn directory_without_r_is_error() {
        let p = plan("grep", &s(&["x", "/etc"]), &|_| true);
        assert_eq!(p.errors, s(&["/etc: Is a directory"]));
        assert!(!p.rg_args.contains(&"/etc".to_string()));
    }
    #[test]
    fn egrep_fgrep_and_clusters() {
        let p = plan("egrep", &s(&["-iv", "a|b", "f"]), &nodir);
        assert_eq!(tail(&p), s(&["-i", "-v", "-e", "a|b", "f"]));
        let p = plan("fgrep", &s(&["a.b", "f"]), &nodir);
        assert_eq!(p.rg_args[6], "-F");
    }
    #[test]
    fn options_with_values() {
        let p = plan("grep", &s(&["-A2", "-m", "1", "-e", "-x", "f"]), &nodir);
        assert_eq!(tail(&p), s(&["-A", "2", "-m", "1", "-e", "-x", "f"]));
        let p = plan("grep", &s(&["-2", "x", "f"]), &nodir);
        assert_eq!(tail(&p), s(&["-C", "2", "-e", "x", "f"]));
        let p = plan("grep", &s(&["--include=*.rs", "-r", "x"]), &nodir);
        assert_eq!(tail(&p), s(&["-g", "*.rs", "-e", "x"]));
    }
    #[test]
    fn missing_pattern_is_usage_error() {
        assert!(plan("grep", &[], &nodir).usage_error.is_some());
        assert!(plan("grep", &s(&["-Q", "x"]), &nodir).usage_error.is_some());
    }
}
