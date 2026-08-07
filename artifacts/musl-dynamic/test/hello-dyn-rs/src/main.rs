fn main() {
    let pid = std::process::id();
    let args: Vec<String> = std::env::args().collect();
    println!("hello-dyn-rs: pid={} argc={}", pid, args.len());
    for (i, a) in args.iter().enumerate() {
        println!("  argv[{}] = {}", i, a);
    }
}
