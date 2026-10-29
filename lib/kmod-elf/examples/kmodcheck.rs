//! Host check run by scripts/kmod/kmod-build.sh on a finished .kmod, with the loader's own
//! validator. usage: kmodcheck <file.kmod> <aarch64|x86_64> <build-id-hex>
use kmod_abi::{layout, EM_AARCH64, EM_X86_64};
use kmod_elf::{validate, Expect};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 4 {
        eprintln!("usage: kmodcheck <file> <aarch64|x86_64> <build-id-hex>");
        std::process::exit(2);
    }
    let img = std::fs::read(&a[1]).expect("read");
    let (machine, base) = match a[2].as_str() {
        "aarch64" => (EM_AARCH64 as u16, layout::aarch64::BASE),
        "x86_64" => (EM_X86_64 as u16, layout::x86_64::BASE),
        _ => panic!("arch"),
    };
    let id: Vec<u8> = (0..a[3].len() / 2).map(|i| u8::from_str_radix(&a[3][2 * i..2 * i + 2], 16).unwrap()).collect();
    let ex = Expect { machine, build_id: &id, window_base: base, window_size: layout::SIZE };
    match validate(&img, &ex) {
        Ok(p) => println!("kmodcheck: ok {} segs, slot {:#x}+{:#x}, init {:#x}, flags {:#x}",
            p.nsegs, p.hdr.slot_base, p.hdr.slot_size, p.hdr.init, p.hdr.flags),
        Err(e) => {
            eprintln!("kmodcheck: REJECTED: {:?}", e);
            std::process::exit(1);
        }
    }
}
