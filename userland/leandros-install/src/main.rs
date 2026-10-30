use leandros_install::mkhome;
use std::process::exit;

fn die(msg: &str) -> ! {
    eprintln!("leandros-install: {msg}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("mkhome") => {
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
            let size = mkhome::parse_size(&size.unwrap_or_else(|| die("--size required")))
                .unwrap_or_else(|e| die(&e));
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
        _ => die("usage: leandros-install mkhome --out FILE --size SIZE [--uuid HEX] [--time EPOCH] [--full]"),
    }
}
