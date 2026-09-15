//! mkfs.xfs for LeandrOS — a from-scratch Rust implementation of the subset of
//! xfsprogs' mkfs.xfs that an installer needs.
//!
//! It writes a v5 (CRC) XFS with the feature set mkfs.xfs 7.x turns on by
//! default, and accepts the xfsprogs command line so that callers which shell
//! out to `mkfs.xfs` — AerynOS' disks-rs among them — need no changes.
//!
//! What it deliberately does not do: multi-device / stripe geometry, external
//! logs, realtime subvolumes, quotas, protofiles, v4 filesystems, or any
//! variation of block/sector/inode size. Options selecting those are accepted
//! and reported as ignored rather than being made fatal.

mod crc32c;
mod dev;
mod format;
mod geom;
mod ondisk;

use std::path::PathBuf;
use std::process::ExitCode;

use dev::Dev;
use format::Params;
use geom::{Features, GeomRequest};

const PROG: &str = "mkfs.xfs";
const VERSION_LINE: &str = "mkfs.xfs version 7.1.1 (LeandrOS mkfs-xfs 0.1.0)";

const USAGE: &str = "\
Usage: mkfs.xfs
/* blocksize */         [-b size=num]
/* metadata */          [-m crc=0|1,finobt=0|1,uuid=xxx,rmapbt=0|1,reflink=0|1,
                            inobtcount=0|1,bigtime=0|1,autofsck=x]
/* data subvol */       [-d agcount=n,agsize=n,file,name=xxx,size=num,
                            sunit=value,swidth=value,su=num,sw=num]
/* inode size */        [-i size=num,maxpct=n,sparse=0|1,nrext64=0|1,
                            exchange=0|1]
/* log subvol */        [-l agnum=n,internal,size=num,version=n,sunit=value,
                            su=num,lazy-count=0|1]
/* label */             [-L label (maximum 12 characters)]
/* naming */            [-n size=num,version=2|ci,ftype=0|1,parent=0|1]
/* sectorsize */        [-s size=num]
/* force overwrite */   [-f]
/* quiet */             [-q]
/* no write */          [-N]
/* version */           [-V]
                        devicename
";

fn warn(msg: &str) {
    eprintln!("{PROG}: warning: {msg}");
}

fn die(msg: &str) -> ExitCode {
    eprintln!("{PROG}: {msg}");
    ExitCode::from(1)
}

// ------------------------------------------------------------------- CLI ----

#[derive(Default)]
struct Cli {
    device: Option<PathBuf>,
    label: String,
    force: bool,
    quiet: bool,
    dry_run: bool,
    uuid: Option<[u8; 16]>,
    autofsck: Option<String>,
    feat: FeatCli,
    dsize: Option<u64>,
    agcount: Option<u64>,
    agsize: Option<u64>,
    logsize: Option<u64>,
    logagno: Option<u32>,
}

#[derive(Default)]
struct FeatCli {
    finobt: Option<bool>,
    sparse: Option<bool>,
    rmapbt: Option<bool>,
    reflink: Option<bool>,
    bigtime: Option<bool>,
    inobtcount: Option<bool>,
    nrext64: Option<bool>,
    exchange: Option<bool>,
    parent: Option<bool>,
    ftype: Option<bool>,
}

/// xfsprogs number syntax: an optional k/m/g/t/p multiplier, or a `b`/`s`
/// suffix meaning "in blocks" / "in sectors". Everything else is bytes.
fn parse_num(v: &str, blocksize: u64, sectorsize: u64) -> Result<u64, String> {
    let s = v.trim();
    if s.is_empty() {
        return Err("empty value".into());
    }
    let (digits, suffix) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("`{v}' is not a number"))?;
    let mult = match suffix.to_ascii_lowercase().as_str() {
        "" => 1,
        "b" => blocksize,
        "s" => sectorsize,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        "p" | "pib" => 1 << 50,
        "e" | "eib" => 1 << 60,
        _ => return Err(format!("`{v}' has an unrecognised size suffix")),
    };
    n.checked_mul(mult).ok_or_else(|| format!("`{v}' overflows"))
}

fn parse_bool(key: &str, v: &str) -> Result<bool, String> {
    match v {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(format!("{key}={v} must be 0 or 1")),
    }
}

fn parse_uuid(s: &str) -> Result<[u8; 16], String> {
    let hex: Vec<u8> = s.bytes().filter(|&b| b != b'-').collect();
    if hex.len() != 32 || s.len() != 36 {
        return Err(format!("`{s}' is not a valid UUID"));
    }
    let mut out = [0u8; 16];
    for (i, pair) in hex.chunks(2).enumerate() {
        let hi = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| format!("`{s}' is not a valid UUID"))?;
        let lo = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| format!("`{s}' is not a valid UUID"))?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Ok(out)
}

fn suboption(cli: &mut Cli, opt: char, spec: &str) -> Result<(), String> {
    for item in spec.split(',').filter(|s| !s.is_empty()) {
        let (key, val) = match item.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (item, None),
        };
        let v = val.unwrap_or("");
        match (opt, key) {
            // ---- values we honour -------------------------------------
            ('m', "uuid") => cli.uuid = Some(parse_uuid(v)?),
            ('m', "autofsck") => match v {
                "none" | "check" | "optimize" | "repair" => cli.autofsck = Some(v.to_string()),
                _ => warn(&format!("ignoring unknown -m autofsck={v}")),
            },
            ('m', "finobt") => cli.feat.finobt = Some(parse_bool(key, v)?),
            ('m', "rmapbt") => cli.feat.rmapbt = Some(parse_bool(key, v)?),
            ('m', "reflink") => cli.feat.reflink = Some(parse_bool(key, v)?),
            ('m', "bigtime") => cli.feat.bigtime = Some(parse_bool(key, v)?),
            ('m', "inobtcount") => cli.feat.inobtcount = Some(parse_bool(key, v)?),
            ('m', "crc") => {
                if !parse_bool(key, v)? {
                    return Err("this mkfs.xfs only writes v5 filesystems; -m crc=0 \
                                is not supported"
                        .into());
                }
            }
            ('i', "sparse") => cli.feat.sparse = Some(parse_bool(key, v)?),
            ('i', "nrext64") => cli.feat.nrext64 = Some(parse_bool(key, v)?),
            ('i', "exchange") => cli.feat.exchange = Some(parse_bool(key, v)?),
            ('n', "parent") => cli.feat.parent = Some(parse_bool(key, v)?),
            ('n', "ftype") => cli.feat.ftype = Some(parse_bool(key, v)?),
            ('d', "size") => cli.dsize = Some(parse_num(v, 4096, 512)? / 4096),
            ('d', "agcount") => cli.agcount = Some(parse_num(v, 1, 1)?),
            ('d', "agsize") => cli.agsize = Some(parse_num(v, 4096, 512)? / 4096),
            ('l', "size") => cli.logsize = Some(parse_num(v, 4096, 512)? / 4096),
            ('l', "agnum") => {
                cli.logagno = Some(parse_num(v, 1, 1)? as u32);
            }
            ('l', "internal") => {
                if val.is_some() && !parse_bool(key, v)? {
                    return Err("external logs are not supported".into());
                }
            }

            // ---- values that must match what we can write --------------
            ('b', "size") => must_equal(opt, key, parse_num(v, 4096, 512)?, 4096),
            ('b', "log") => must_equal(opt, key, parse_num(v, 1, 1)?, 12),
            ('s', "size") => must_equal(opt, key, parse_num(v, 4096, 512)?, 512),
            ('s', "log") => must_equal(opt, key, parse_num(v, 1, 1)?, 9),
            ('i', "size") => must_equal(opt, key, parse_num(v, 4096, 512)?, 512),
            ('i', "log") => must_equal(opt, key, parse_num(v, 1, 1)?, 9),
            ('n', "size") => must_equal(opt, key, parse_num(v, 4096, 512)?, 4096),
            ('n', "version") => must_equal_str(opt, key, v, "2"),
            ('l', "version") => must_equal_str(opt, key, v, "2"),
            ('l', "lazy-count") => must_equal_str(opt, key, v, "1"),
            ('i', "maxpct") => must_equal_str(opt, key, v, "25"),
            ('i', "align") => must_equal_str(opt, key, v, "1"),
            ('i', "projid32bit") => must_equal_str(opt, key, v, "1"),
            ('i', "attr") => must_equal_str(opt, key, v, "2"),

            // ---- accepted and ignored ----------------------------------
            ('d', "noalign") | ('d', "file") | ('d', "name") | ('d', "sunit")
            | ('d', "swidth") | ('d', "su") | ('d', "sw") | ('d', "concurrency")
            | ('d', "rtinherit") | ('d', "cowextsize") | ('d', "extszinherit")
            | ('d', "daxinherit") | ('l', "sunit") | ('l', "su") | ('l', "concurrency")
            | ('m', "metadir") | ('i', "perblock") | ('i', "max_atomic_write") => {
                warn(&format!("-{opt} {item}: not supported, ignored"))
            }

            _ => warn(&format!("-{opt} {item}: unknown option, ignored")),
        }
    }
    Ok(())
}

fn must_equal(opt: char, key: &str, got: u64, want: u64) {
    if got != want {
        warn(&format!(
            "-{opt} {key}={got}: only {want} is supported, ignored"
        ));
    }
}

fn must_equal_str(opt: char, key: &str, got: &str, want: &str) {
    if got != want {
        warn(&format!(
            "-{opt} {key}={got}: only {want} is supported, ignored"
        ));
    }
}

fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if let Some(rest) = a.strip_prefix('-') {
            let mut chars = rest.chars();
            let c = match chars.next() {
                Some(c) => c,
                None => return Err("stray `-' on the command line".into()),
            };
            let glued: String = chars.collect();
            // Options in this set take a value, either glued to the letter or
            // as the next argument, exactly as xfsprogs' getopt does.
            let value = if "bdilmnsrLp".contains(c) {
                if glued.is_empty() {
                    let v = args
                        .get(i)
                        .ok_or_else(|| format!("-{c} needs an argument"))?
                        .clone();
                    i += 1;
                    Some(v)
                } else {
                    Some(glued)
                }
            } else {
                None
            };
            match c {
                'b' | 'd' | 'i' | 'l' | 'm' | 'n' | 's' | 'r' => {
                    let spec = value.unwrap();
                    if c == 'r' {
                        warn("-r: realtime subvolumes are not supported, ignored");
                    } else {
                        suboption(&mut cli, c, &spec)?;
                    }
                }
                'L' => cli.label = value.unwrap(),
                'p' => warn("-p: protofiles are not supported, ignored"),
                'f' => cli.force = true,
                'q' => cli.quiet = true,
                'N' => cli.dry_run = true,
                'K' | 'c' => {}
                'V' => {
                    println!("{VERSION_LINE}");
                    std::process::exit(0);
                }
                'h' => {
                    print!("{USAGE}");
                    std::process::exit(0);
                }
                _ => warn(&format!("-{c}: unknown option, ignored")),
            }
        } else if cli.device.is_none() {
            cli.device = Some(PathBuf::from(a));
        } else {
            return Err(format!("unexpected extra argument `{a}'"));
        }
    }
    Ok(cli)
}

// ------------------------------------------------------------- randomness ---

fn random_bytes(n: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut v = vec![0u8; n];
    f.read_exact(&mut v)?;
    Ok(v)
}

fn random_uuid() -> [u8; 16] {
    let mut u = [0u8; 16];
    match random_bytes(16) {
        Ok(r) => u.copy_from_slice(&r),
        Err(_) => {
            // No /dev/urandom: fall back to the clock. A UUID collision is not
            // a correctness problem for a filesystem that is about to be
            // written; a hard failure here would be.
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            u[0..8].copy_from_slice(&t.to_le_bytes());
            u[8..16].copy_from_slice(&t.rotate_left(29).to_be_bytes());
        }
    }
    u[6] = (u[6] & 0x0f) | 0x40; // version 4
    u[8] = (u[8] & 0x3f) | 0x80; // RFC 4122 variant
    u
}

fn fmt_uuid(u: &[u8; 16]) -> String {
    let h = |r: &[u8]| r.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        h(&u[0..4]),
        h(&u[4..6]),
        h(&u[6..8]),
        h(&u[8..10]),
        h(&u[10..16])
    )
}

// ------------------------------------------------------------------ main ----

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{PROG}: {e}");
            eprint!("{USAGE}");
            return ExitCode::from(1);
        }
    };

    let device = match &cli.device {
        Some(d) => d.clone(),
        None => {
            eprint!("{USAGE}");
            return ExitCode::from(1);
        }
    };

    if cli.label.len() > 12 {
        return die(&format!(
            "label `{}' is too long, maximum 12 characters",
            cli.label
        ));
    }

    let d = Features::default();
    let mut feat = Features {
        ftype: cli.feat.ftype.unwrap_or(d.ftype),
        finobt: cli.feat.finobt.unwrap_or(d.finobt),
        sparse: cli.feat.sparse.unwrap_or(d.sparse),
        rmapbt: cli.feat.rmapbt.unwrap_or(d.rmapbt),
        reflink: cli.feat.reflink.unwrap_or(d.reflink),
        bigtime: cli.feat.bigtime.unwrap_or(d.bigtime),
        inobtcount: cli.feat.inobtcount.unwrap_or(d.inobtcount),
        nrext64: cli.feat.nrext64.unwrap_or(d.nrext64),
        exchange: cli.feat.exchange.unwrap_or(d.exchange),
        parent: cli.feat.parent.unwrap_or(d.parent),
    };
    // mkfs turns exchange-range on implicitly when parent pointers are asked
    // for, because online repair of a directory needs both.
    if feat.parent && cli.feat.exchange.is_none() {
        feat.exchange = true;
    }
    if feat.reflink && !feat.finobt {
        return die("reflink requires finobt");
    }

    let mut dev = match Dev::open(&device, cli.dry_run) {
        Ok(d) => d,
        Err(e) => return die(&format!("cannot open {}: {e}", device.display())),
    };
    let devsize = match dev.size() {
        Ok(0) => return die(&format!("cannot determine the size of {}", device.display())),
        Ok(s) => s,
        Err(e) => return die(&format!("cannot determine the size of {}: {e}", device.display())),
    };

    if !cli.force && !cli.dry_run {
        match existing_xfs(&dev) {
            Ok(true) => {
                return die(&format!(
                    "{} appears to contain an existing filesystem; use -f to overwrite",
                    device.display()
                ))
            }
            Ok(false) => {}
            Err(e) => return die(&format!("cannot read {}: {e}", device.display())),
        }
    }

    let geom = match geom::compute(&GeomRequest {
        devsize,
        feat,
        dsize_blocks: cli.dsize,
        agcount: cli.agcount,
        agsize: cli.agsize,
        logsize_blocks: cli.logsize,
        logagno: cli.logagno,
    }) {
        Ok(g) => g,
        Err(e) => return die(&e),
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let gen = u32::from_le_bytes(
        random_bytes(4)
            .map(|v| [v[0], v[1], v[2], v[3]])
            .unwrap_or([0; 4]),
    );

    let params = Params {
        geom,
        uuid: cli.uuid.unwrap_or_else(random_uuid),
        label: cli.label.clone(),
        autofsck: cli.autofsck.clone(),
        now_sec: now.as_secs() as i64,
        now_nsec: now.subsec_nanos(),
        gen,
    };

    if !cli.quiet {
        print_geometry(&device.display().to_string(), &params);
    }
    if cli.dry_run {
        return ExitCode::SUCCESS;
    }

    match params.write(&dev) {
        Ok(_) => {}
        Err(e) => return die(&format!("write to {} failed: {e}", device.display())),
    }
    // Nothing else goes to stdout: callers parse the geometry table above and
    // xfsprogs prints nothing after it on success.
    if !cli.quiet {
        eprintln!(
            "{PROG}: wrote {} KiB of metadata, UUID {}",
            dev.bytes_written() / 1024,
            fmt_uuid(&params.uuid)
        );
    }
    ExitCode::SUCCESS
}

/// Refuse to clobber something that is already a filesystem unless -f was
/// given. Only XFS is recognised — that is what mkfs.xfs' own check without
/// libblkid amounts to, and the installer always passes -f anyway.
fn existing_xfs(dev: &Dev) -> std::io::Result<bool> {
    let mut sect = [0u8; 512];
    let n = dev.read_at(0, &mut sect)?;
    if n < 4 {
        return Ok(false);
    }
    Ok(u32::from_be_bytes(sect[0..4].try_into().unwrap()) == ondisk::XFS_SB_MAGIC)
}

fn print_geometry(name: &str, p: &Params) {
    let g = &p.geom;
    let b = |v: bool| u8::from(v);
    print!(
        "meta-data={:<22} isize={:<6} agcount={}, agsize={} blks\n\
         \x20        ={:<22} sectsz={:<5} attr={}, projid32bit={}\n\
         \x20        ={:<22} crc={:<8} finobt={}, sparse={}, rmapbt={}\n\
         \x20        ={:<22} reflink={:<4} bigtime={} inobtcount={} nrext64={}\n\
         \x20        ={:<22} exchange={:<3} metadir={}\n\
         data     ={:<22} bsize={:<6} blocks={}, imaxpct={}\n\
         \x20        ={:<22} sunit={:<6} swidth={} blks\n\
         naming   =version {:<14} bsize={:<6} ascii-ci={}, ftype={}, parent={}\n\
         log      ={:<22} bsize={:<6} blocks={}, version={}\n\
         \x20        ={:<22} sectsz={:<5} sunit={} blks, lazy-count={}\n\
         realtime ={:<22} extsz={:<6} blocks={}, rtextents={}\n\
         \x20        ={:<22} rgcount={:<4} rgsize={} extents\n\
         \x20        ={:<22} zoned={:<6} start={} reserved={}\n",
        name,
        g.inodesize,
        g.agcount,
        g.agblocks,
        "",
        g.sectorsize,
        2,
        1,
        "",
        1,
        b(g.feat.finobt),
        b(g.feat.sparse),
        b(g.feat.rmapbt),
        "",
        b(g.feat.reflink),
        b(g.feat.bigtime),
        b(g.feat.inobtcount),
        b(g.feat.nrext64),
        "",
        b(g.feat.exchange),
        0,
        "",
        g.blocksize,
        g.dblocks,
        g.imaxpct,
        "",
        0,
        0,
        2,
        g.blocksize,
        0,
        b(g.feat.ftype),
        b(g.feat.parent),
        "internal log",
        g.blocksize,
        g.logblocks,
        2,
        "",
        g.sectorsize,
        0,
        1,
        "none",
        g.blocksize,
        0,
        0,
        "",
        0,
        0,
        "",
        0,
        0,
        0,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disks_rs_command_line_parses() {
        let args: Vec<String> = [
            "-m",
            "uuid=01234567-89ab-cdef-0123-456789abcdef",
            "-L",
            "ROOT",
            "-n",
            "parent=1",
            "-i",
            "exchange=1",
            "-m",
            "autofsck=repair",
            "-f",
            "/dev/sda2",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let cli = parse_args(&args).unwrap();
        assert_eq!(cli.device.unwrap().to_str().unwrap(), "/dev/sda2");
        assert_eq!(cli.label, "ROOT");
        assert!(cli.force);
        assert_eq!(cli.feat.parent, Some(true));
        assert_eq!(cli.feat.exchange, Some(true));
        assert_eq!(cli.autofsck.as_deref(), Some("repair"));
        assert_eq!(cli.uuid.unwrap()[0], 0x01);
        assert_eq!(cli.uuid.unwrap()[15], 0xef);
    }

    #[test]
    fn unknown_suboptions_warn_but_do_not_fail() {
        let args: Vec<String> = ["-d", "su=64k,sw=4,wibble=3", "-l", "lazy-count=1", "/dev/x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_args(&args).is_ok());
    }

    #[test]
    fn sizes_take_suffixes() {
        assert_eq!(parse_num("4096", 4096, 512).unwrap(), 4096);
        assert_eq!(parse_num("64m", 4096, 512).unwrap(), 64 << 20);
        assert_eq!(parse_num("16b", 4096, 512).unwrap(), 16 * 4096);
        assert_eq!(parse_num("8s", 4096, 512).unwrap(), 8 * 512);
    }

    #[test]
    fn uuid_round_trip() {
        let u = parse_uuid("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        assert_eq!(fmt_uuid(&u), "01234567-89ab-cdef-0123-456789abcdef");
    }
}
