use leandros_install::install::{self, Opts};
use leandros_install::mkhome;
use leandros_install::progress::Out;
use leandros_install::shrink;
fn bye(code: i32) -> ! {
    install::restore_ro();
    std::process::exit(code)
}

fn die(msg: &str) -> ! {
    eprintln!("leandros-install: {msg}");
    bye(1);
}

const USAGE: &str = "usage:
  leandros-install list [--json] [--allow-removable]
  leandros-install install --target /dev/X --yes [--root-gib N] [--allow-removable]
                           [--no-verify] [--zero-fill] [--json] [--dry-run]
  leandros-install install --mode dualboot --target /dev/X --use-free --yes [--root-gib N]
                           [--json] [--dry-run]   (installs only into unallocated space)
  leandros-install install --mode dualboot --target /dev/X --after-shrink --yes [--root-gib N]
                           [--json] [--dry-run]   (session B: needs shrink-status ready_b; shrinks the
                           p2 GPT entry to the shrunk filesystem, then installs into the freed gap)
  leandros-install repair-esp [--boot DEV] [--esp DEV]
      recreate the three LeandrOS files on a dual-boot disk's shared ESP (runs espctl repair-esp)
  leandros-install gaps --dev FILE|/dev/X [--sector-size 512|4096]
  leandros-install shrink --part /dev/Xp2 --to SIZE [--dry-run] --yes [--json]
      session A of the dual-boot ext4 shrink (filesystem only, GPT untouched)
  leandros-install shrink --recover [--part DEV] [--dir D] --yes [--json]   (e2undo after a crash)
  leandros-install shrink-status [--dir D] [--mark-verified] [--json]
  leandros-install mkhome --out FILE --size SIZE [--uuid HEX] [--time EPOCH] [--full]";

fn mkhome_cmd(args: &[String]) {
    let (mut out, mut size, mut uuid, mut time, mut full) = (None, None, None, None, false);
    let mut i = 1;
    while i < args.len() {
        let val = |i: usize| args.get(i + 1).cloned().unwrap_or_else(|| die("missing value"));
        match args[i].as_str() {
            "--out" => { out = Some(val(i)); i += 2; }
            "--size" => { size = Some(val(i)); i += 2; }
            "--uuid" => { uuid = Some(val(i)); i += 2; }
            "--time" => { time = Some(val(i)); i += 2; }
            "--full" => { full = true; i += 1; }
            a => die(&format!("unknown argument {a}")),
        }
    }
    let out = out.unwrap_or_else(|| die("--out required"));
    let size = mkhome::parse_size(&size.unwrap_or_else(|| die("--size required"))).unwrap_or_else(|e| die(&e));
    let uuid = match uuid {
        Some(u) => mkhome::parse_uuid(&u).unwrap_or_else(|e| die(&e)),
        None => mkhome::random_uuid().unwrap_or_else(|e| die(&e.to_string())),
    };
    let time = match time {
        Some(t) => t.parse().unwrap_or_else(|_| die("bad --time")),
        None => mkhome::now(),
    };
    if let Err(e) = mkhome::write_home(std::path::Path::new(&out), size, &uuid, time, !full) {
        die(&e.to_string());
    }
}

fn gaps_cmd(args: &[String]) {
    let (mut dev, mut ss) = (None, 512u64);
    let mut i = 1;
    while i < args.len() {
        let val = args.get(i + 1).cloned().unwrap_or_else(|| die("missing value"));
        match args[i].as_str() {
            "--dev" => dev = Some(val),
            "--sector-size" => ss = val.parse().unwrap_or_else(|_| die("bad --sector-size")),
            a => die(&format!("unknown argument {a}")),
        }
        i += 2;
    }
    let dev = dev.unwrap_or_else(|| die("--dev required"));
    let mut f = std::fs::File::open(&dev).unwrap_or_else(|e| die(&format!("{dev}: {e}")));
    let len = std::io::Seek::seek(&mut f, std::io::SeekFrom::End(0)).unwrap_or_else(|e| die(&e.to_string()));
    let g = leandros_install::gpt::Gpt::read(&mut f, ss, len / ss).unwrap_or_else(|e| die(&e));
    for (i, e) in g.used() {
        println!("p{} {}..{} {}", i + 1, e.first, e.last, e.name);
    }
    for (a, b) in g.gaps() {
        println!("gap {}..{} ({} MiB)", a, b, (b - a + 1) * ss >> 20);
    }
}

fn parse_opts(args: &[String]) -> Opts {
    let mut o = Opts::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--target" => { o.target = Some(args.get(i + 1).cloned().unwrap_or_else(|| die("missing value"))); i += 2; }
            "--root-gib" => {
                o.root_gib = Some(args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or_else(|| die("bad --root-gib")));
                i += 2;
            }
            "--yes" => { o.yes = true; i += 1; }
            "--dry-run" => { o.dry_run = true; i += 1; }
            "--allow-removable" => { o.allow_removable = true; i += 1; }
            "--no-verify" => { o.no_verify = true; i += 1; }
            "--zero-fill" => { o.zero_fill = true; i += 1; }
            "--json" => { o.json = true; i += 1; }
            "--use-free" => { o.use_free = true; i += 1; }
            "--after-shrink" => { o.after_shrink = true; o.use_free = true; i += 1; }
            "--backup-root" => { o.backup_root = Some(args.get(i + 1).cloned().unwrap_or_else(|| die("missing value"))); i += 2; }
            "--mode" => {
                match args.get(i + 1).map(String::as_str) {
                    Some("dualboot") => o.dualboot = true,
                    Some("wipe") => o.dualboot = false,
                    _ => die("--mode must be wipe or dualboot"),
                }
                i += 2;
            }
            a => die(&format!("unknown argument {a}")),
        }
    }
    o
}

fn shrink_cmd(args: &[String]) -> shrink::Opts {
    let mut o = shrink::Opts::default();
    let mut i = 1;
    while i < args.len() {
        let val = |i: usize| args.get(i + 1).cloned().unwrap_or_else(|| die("missing value"));
        match args[i].as_str() {
            "--part" => { o.part = Some(val(i)); i += 2; }
            "--to" => { o.to = Some(mkhome::parse_size(&val(i)).unwrap_or_else(|e| die(&e))); i += 2; }
            "--dir" => { o.dir = Some(val(i)); i += 2; }
            "--backup-root" => { o.root = Some(val(i)); i += 2; }
            "--dry-run" => { o.dry_run = true; i += 1; }
            "--yes" => { o.yes = true; i += 1; }
            "--json" => { o.json = true; i += 1; }
            "--check-sb" => { o.check_sb = Some(val(i)); i += 2; }
            "--recover" => { o.recover = true; i += 1; }
            "--mark-verified" => { o.mark_verified = true; i += 1; }
            a => die(&format!("unknown argument {a}")),
        }
    }
    o
}

fn main() {
    let _ro = install::RoGuard;
    run_main();
}

fn run_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("repair-esp") => {
            // The ESP logic lives in espctl (fatfs, owner markers); one implementation.
            let st = std::process::Command::new("espctl").arg("repair-esp").args(&args[1..]).status();
            match st {
                Ok(s) if s.success() => {}
                Ok(s) => bye(s.code().unwrap_or(1)),
                Err(e) => die(&format!("cannot run espctl: {e}")),
            }
        }
        Some("shrink") | Some("shrink-status") => {
            let o = shrink_cmd(&args);
            let r = if args[0] == "shrink" { shrink::shrink(&o) } else { shrink::status(&o) };
            if let Err(e) = r {
                Out { json: o.json }.error(&e);
                bye(1);
            }
        }
        Some("gaps") => gaps_cmd(&args),
        Some("mkhome") => mkhome_cmd(&args),
        Some("list") => install::list(&parse_opts(&args)),
        Some("install") => {
            let o = parse_opts(&args);
            if let Err(e) = install::install(&o) {
                Out { json: o.json }.error(&e);
                bye(1);
            }
        }
        _ => die(USAGE),
    }
}
