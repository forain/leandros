// M7b tokio-layer repro for the LeandrOS "freshly-spawned ready task never
// polled" divergence (W1). Mirrors busd: a runtime settles into the I/O driver
// park (epoll_wait INFINITE), then a task is spawned that should be driven to
// its first poll. If R:POLLED never prints, the bug is reproduced.
//
// argv[1] = variant: A (spawn from within a runtime task, triggered by an
//                       AF_UNIX connect — exact busd shape)
//                    B (spawn from a foreign std::thread via Handle::spawn —
//                       exercises tokio's waker-eventfd unpark path)
// argv[2] = flavor:  ct (current_thread, default) | mt (multi_thread)
//
// Markers are emitted two ways: (1) a println to stderr for the human log, and
// (2) a magic prctl(0x6d37c, id) NOP that appears verbatim in the kernel syscall
// trace AND in Alpine strace — so the two traces can be aligned marker-by-marker.

use std::io::Write;
use std::time::Duration;

const P_MARK: i32 = 0x6d37c; // magic prctl: pure marker, id in arg2
const P_ARM: i32 = 0x6d37b; // magic prctl: arm(1)/disarm(0) kernel rich trace
const P_DUMP: i32 = 0x6d37d; // magic prctl: dump the kernel trace ring to serial

fn dump_trace() {
    unsafe {
        libc::prctl(P_DUMP, 0, 0, 0, 0);
    }
}

// ── Cross-thread futex divergence test ───────────────────────────────────────
// Tests whether a cross-thread FUTEX_WAKE reaches a TIMED futex waiter. On Linux
// FUTEX_WAKE wakes timed and untimed waiters identically. If LeandrOS's timed
// futex_wait yield-loops without registering, a pure FUTEX_WAKE (no change to the
// futex word) is LOST and the waiter sleeps its full timeout.
fn ftx_wait(addr: &std::sync::atomic::AtomicU32, expected: u32, timeout_ms: Option<u64>) -> i64 {
    let ts;
    let tptr = match timeout_ms {
        Some(ms) => {
            ts = libc::timespec { tv_sec: (ms / 1000) as libc::time_t,
                                  tv_nsec: ((ms % 1000) * 1_000_000) as libc::c_long };
            &ts as *const libc::timespec as usize
        }
        None => 0usize,
    };
    unsafe {
        libc::syscall(libc::SYS_futex, addr as *const _ as usize,
                      libc::FUTEX_WAIT, expected as usize, tptr, 0usize, 0usize)
    }
}
fn ftx_wake(addr: &std::sync::atomic::AtomicU32, n: i32) -> i64 {
    unsafe {
        libc::syscall(libc::SYS_futex, addr as *const _ as usize,
                      libc::FUTEX_WAKE, n as usize, 0usize, 0usize, 0usize)
    }
}
fn futextest() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};
    use std::sync::Arc;
    let _ = writeln!(std::io::stderr(), "FUTEXTEST: start");
    let mut fails = 0;

    // Subtest 1: UNTIMED cross-thread wake WITH value change (control).
    {
        let w = Arc::new(AtomicU32::new(0));
        let w2 = w.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            w2.store(1, Ordering::SeqCst);
            ftx_wake(&w2, 1);
        });
        let t0 = Instant::now();
        while w.load(Ordering::SeqCst) == 0 { ftx_wait(&w, 0, None); }
        let el = t0.elapsed().as_millis();
        h.join().ok();
        let ok = el < 2000;
        if !ok { fails += 1; }
        let _ = writeln!(std::io::stderr(), "T1 untimed+valchange: elapsed={}ms {}", el, if ok {"PASS"} else {"FAIL"});
    }
    // Subtest 2: TIMED cross-thread wake WITH value change (should pass via poll).
    {
        let w = Arc::new(AtomicU32::new(0));
        let w2 = w.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            w2.store(1, Ordering::SeqCst);
            ftx_wake(&w2, 1);
        });
        let t0 = Instant::now();
        while w.load(Ordering::SeqCst) == 0 { ftx_wait(&w, 0, Some(5000)); if t0.elapsed().as_millis()>=4900 {break;} }
        let el = t0.elapsed().as_millis();
        h.join().ok();
        let ok = el < 2000;
        if !ok { fails += 1; }
        let _ = writeln!(std::io::stderr(), "T2 timed+valchange:  elapsed={}ms {}", el, if ok {"PASS"} else {"FAIL"});
    }
    // Subtest 3: TIMED cross-thread wake WITHOUT value change (THE DISCRIMINATOR).
    // Linux: FUTEX_WAKE wakes the timed waiter -> returns ~600ms. LeandrOS bug:
    // timed waiter unregistered -> wake lost -> waits full 5000ms timeout.
    {
        let w = Arc::new(AtomicU32::new(0));
        let w2 = w.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            ftx_wake(&w2, 1); // NO value change — pure wake
        });
        let t0 = Instant::now();
        ftx_wait(&w, 0, Some(5000)); // single call; measure how long it blocks
        let el = t0.elapsed().as_millis();
        h.join().ok();
        let ok = el < 2000;
        if !ok { fails += 1; }
        let _ = writeln!(std::io::stderr(), "T3 timed+PUREWAKE:   elapsed={}ms {}  <-- cross-thread FUTEX_WAKE of timed waiter", el, if ok {"PASS"} else {"FAIL(wake lost)"});
    }
    let _ = writeln!(std::io::stderr(), "FUTEXTEST: fails={}", fails);
    std::process::exit(if fails==0 {0} else {1});
}

// ── coalescing D-Bus client (desktop-free W1 repro; mirrors client.py) ─────────
fn align(v: &mut Vec<u8>, n: usize) { while v.len() % n != 0 { v.push(0); } }
fn marshal_string(s: &str) -> Vec<u8> {
    let e = s.as_bytes();
    let mut o = (e.len() as u32).to_le_bytes().to_vec();
    o.extend_from_slice(e); o.push(0); o
}
fn marshal_sig(s: &str) -> Vec<u8> {
    let e = s.as_bytes();
    let mut o = vec![e.len() as u8];
    o.extend_from_slice(e); o.push(0); o
}
fn header_field(code: u8, sig: &str, val: &str) -> Vec<u8> {
    let mut b = vec![code];
    b.extend_from_slice(&marshal_sig(sig));
    align(&mut b, 4);
    b.extend_from_slice(&marshal_string(val));
    b
}
// A no-body METHOD_CALL to org.freedesktop.DBus.<member> at serial `serial`.
fn method_call(member: &str, serial: u32) -> Vec<u8> {
    let mut fields: Vec<u8> = Vec::new();
    for (code, sig, val) in [
        (1u8, "o", "/org/freedesktop/DBus"),
        (6u8, "s", "org.freedesktop.DBus"),
        (2u8, "s", "org.freedesktop.DBus"),
        (3u8, "s", member),
    ] {
        align(&mut fields, 8);
        fields.extend_from_slice(&header_field(code, sig, val));
    }
    let body: Vec<u8> = Vec::new();
    let mut hdr = vec![b'l', 1u8, 0u8, 1u8];
    hdr.extend_from_slice(&(body.len() as u32).to_le_bytes());
    hdr.extend_from_slice(&serial.to_le_bytes());
    hdr.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    hdr.extend_from_slice(&fields);
    align(&mut hdr, 8);
    hdr.extend_from_slice(&body);
    hdr
}
fn hello_message() -> Vec<u8> { method_call("Hello", 1) }
fn coalclient() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let path = std::env::args().nth(2).unwrap_or_else(|| "/run/user/0/bus".into());
    let uid = unsafe { libc::getuid() };
    let uid_s = format!("{}", uid);
    let hexuid: String = uid_s.bytes().map(|c| format!("{:02x}", c)).collect();
    let authline = format!("AUTH EXTERNAL {}\r\n", hexuid);
    let mut s = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => { eprintln!("coalclient: connect {} failed: {:?}", path, e); std::process::exit(1); }
    };
    // Optional 3rd arg = number of extra GetId method calls PIPELINED after Hello
    // in the SAME coalesced blob. This mimics a real client (cosmic-comp) that
    // pipelines follow-up calls, leaving residual buffered data present when busd
    // spawns the per-peer socket_reader (no fresh readability edge after Hello).
    let npipe: u32 = std::env::args().nth(3).and_then(|a| a.parse().ok()).unwrap_or(0);
    let mut blob: Vec<u8> = vec![0u8];
    blob.extend_from_slice(authline.as_bytes());
    blob.extend_from_slice(b"NEGOTIATE_UNIX_FD\r\n");
    blob.extend_from_slice(b"BEGIN\r\n");
    blob.extend_from_slice(&hello_message());
    for i in 0..npipe {
        blob.extend_from_slice(&method_call("GetId", 2 + i));
    }
    eprintln!("coalclient: COALESCED send {} bytes to {} (npipe={})", blob.len(), path, npipe);
    if let Err(e) = s.write_all(&blob) { eprintln!("coalclient: write failed {:?}", e); std::process::exit(1); }
    // Read the FULL handshake reply (OK + AGREE_UNIX_FD + Hello MethodReturn),
    // NOT quitting early — a real peer (comp) drains everything.
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    let mut buf = [0u8; 1024];
    let mut total = 0usize;
    let mut got_hello = false;
    loop {
        match s.read(&mut buf) {
            Ok(0) => { eprintln!("coalclient: server EOF"); break; }
            Ok(n) => {
                total += n;
                // The Hello MethodReturn is a binary D-Bus msg (starts with 'l',
                // type 2). The auth replies are ASCII "OK ..\r\n" / "AGREE..\r\n".
                if buf[..n].windows(1).any(|w| w[0] == b'l') && total > 40 { got_hello = true; }
                eprintln!("coalclient: recv {} bytes (total {}{})", n, total,
                          if got_hello { ", HELLO seen" } else { "" });
                if got_hello { break; }
            }
            Err(_) => { eprintln!("coalclient: read timeout at total={}", total); break; }
        }
    }
    eprintln!("coalclient: HANDSHAKE {} ({}B)", if got_hello { "HELLO_ANSWERED" } else { "INCOMPLETE(wedge?)" }, total);
    // CRITICAL: STAY CONNECTED. A real peer (comp) keeps the socket open, so busd
    // spawns a per-peer socket_reader that parks awaiting the NEXT message. That
    // first poll of the freshly-spawned reader is the M7e wedge locus. Hold the fd
    // open 18s so the wedge (if any) is observable in busd's ring + log.
    eprintln!("coalclient: holding connection open 18s (socket_reader wedge window)");
    std::thread::sleep(Duration::from_secs(18));
    eprintln!("coalclient: closing");
    drop(s);
}

fn mark(id: u64) {
    unsafe {
        libc::prctl(P_MARK, id as libc::c_ulong, 0, 0, 0);
    }
    let _ = writeln!(std::io::stderr(), "MARK {}", id);
}
fn arm_trace(on: bool) {
    unsafe {
        libc::prctl(P_ARM, if on { 1 } else { 0 } as libc::c_ulong, 0, 0, 0);
    }
}

const SOCK: &str = "/tmp/m7repro.sock";

async fn accept_loop() {
    let listener = tokio::net::UnixListener::bind(SOCK).unwrap();
    mark(2); // BOUND — arm the trace here so the window is settle->park->wake->spawn->poll
    arm_trace(true);
    let _ = writeln!(std::io::stderr(), "M:BOUND, entering accept loop");
    mark(3); // PRE-PARK (about to await accept -> runtime parks the I/O driver)
    loop {
        let accepted = listener.accept().await;
        mark(4); // ACCEPTED (the connect wake worked — proven kernel path)
        match accepted {
            Ok((mut sock, _)) => {
                let _ = writeln!(std::io::stderr(), "M:ACCEPTED, spawning reader");
                tokio::spawn(async move {
                    // THE CRITICAL MARKER: does the freshly-spawned task get its
                    // first poll while the runtime is settled in the park?
                    mark(6); // R:POLLED
                    let _ = writeln!(std::io::stderr(), "R:POLLED (reader first poll)");
                    use tokio::io::AsyncReadExt;
                    let mut buf = [0u8; 64];
                    match sock.read(&mut buf).await {
                        Ok(n) => {
                            let _ = writeln!(std::io::stderr(), "R:READ {} bytes", n);
                        }
                        Err(e) => {
                            let _ = writeln!(std::io::stderr(), "R:ERR {:?}", e);
                        }
                    }
                    mark(7); // R:DONE
                    let _ = writeln!(std::io::stderr(), "R:DONE — SUCCESS, exiting 0");
                    dump_trace();
                    arm_trace(false);
                    std::process::exit(0);
                });
                mark(5); // SPAWNED (runtime should now drive the reader to poll)
                let _ = writeln!(std::io::stderr(), "M:SPAWNED, looping back to accept");
            }
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "M:ACCEPT ERR {:?}", e);
                return;
            }
        }
    }
}

fn main() {
    let variant = std::env::args().nth(1).unwrap_or_else(|| "A".into());

    // Instrumentation subcommands for tracing an ARBITRARY program (busd):
    //   m7repro armexec <prog> <args...>  -> arm trace (sets TRACE_TGID=this
    //     tgid, resets ring) then execve <prog>; tgid is preserved across
    //     execve so <prog> runs traced.
    //   m7repro dump                      -> dump the global ring to serial
    //     (ring is process-independent, so any process can trigger it).
    if variant == "armexec" {
        unsafe { libc::prctl(P_ARM, 1u64 as libc::c_ulong, 0, 0, 0); }
        use std::ffi::CString;
        let args: Vec<CString> = std::env::args().skip(2)
            .map(|a| CString::new(a).unwrap()).collect();
        if args.is_empty() { eprintln!("armexec: need a program"); std::process::exit(2); }
        let mut ptrs: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        unsafe { libc::execv(ptrs[0], ptrs.as_ptr()); }
        eprintln!("armexec: execv failed");
        std::process::exit(127);
    }
    if variant == "dump" {
        dump_trace();
        return;
    }
    // coalclient <sockpath> [ndelay_ms]: connect to a unix D-Bus socket and send
    // the AUTH+NEGOTIATE_UNIX_FD+BEGIN+Hello block COALESCED in one write (exactly
    // like cosmic-comp), then recv the reply. Desktop-free W1 repro (M7f plan).
    if variant == "coalclient" {
        coalclient();
        return;
    }
    if variant == "futextest" {
        futextest();
        return;
    }

    let flavor = std::env::args().nth(2).unwrap_or_else(|| "ct".into());
    let _ = writeln!(
        std::io::stderr(),
        "M:START variant={} flavor={}",
        variant,
        flavor
    );
    mark(1); // START
    let _ = std::fs::remove_file(SOCK);

    // Watchdog: bound the whole test. If the reader never polls, we exit(2).
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(9));
        let _ = writeln!(std::io::stderr(), "WATCHDOG:TIMEOUT — reader never polled, FAIL");
        mark(11);
        dump_trace();
        arm_trace(false);
        std::process::exit(2);
    });

    let rt = if flavor == "mt" {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    };
    let handle = rt.handle().clone();

    // Trigger thread: waits ~1.2s for the runtime to settle into the park, then
    // stimulates the spawn. Variant A: connect to the listener (wakes the
    // accept task from within the runtime, which then spawns the reader).
    // Variant B: directly spawn a task from this foreign thread (waker eventfd).
    let v = variant.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1200));
        mark(8); // TRIGGER thread active
        if v == "B" {
            let _ = writeln!(std::io::stderr(), "T:FOREIGN-SPAWN");
            mark(10);
            handle.spawn(async {
                mark(6); // reuse R:POLLED id — foreign-spawned task ran
                let _ = writeln!(std::io::stderr(), "B:POLLED (foreign-spawned task ran)");
                mark(7);
                let _ = writeln!(std::io::stderr(), "B:DONE — SUCCESS, exiting 0");
                dump_trace();
                arm_trace(false);
                std::process::exit(0);
            });
        } else {
            let _ = writeln!(std::io::stderr(), "T:CONNECT");
            mark(9);
            match std::os::unix::net::UnixStream::connect(SOCK) {
                Ok(mut s) => {
                    let _ = s.write_all(b"hello-from-m7repro");
                    // hold the connection open a while so the reader has data
                    std::thread::sleep(Duration::from_secs(4));
                }
                Err(e) => {
                    let _ = writeln!(std::io::stderr(), "T:CONNECT ERR {:?}", e);
                }
            }
        }
    });

    // Variant B: block_on a future that just sleeps forever (keeps the runtime
    // alive and its I/O driver parked). Variant A: run the accept loop.
    if variant == "B" {
        rt.block_on(async {
            mark(2);
            arm_trace(true);
            let _ = writeln!(std::io::stderr(), "M:variantB parked (sleeping forever)");
            mark(3);
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
    } else {
        rt.block_on(accept_loop());
    }
}
