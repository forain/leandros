// liprobe — drive libinput directly, with its own diagnostics turned all the way
// up, and report every event it does or does not produce.
//
// WHY THIS EXISTS. The M13 census established, on a live COSMIC session:
//   * the libudev shim enumerates /sys/class/input/event{0,1}, both
//     is_initialized, both ID_SEAT=seat0, both tagged (ID_INPUT=1 plus
//     ID_INPUT_KEYBOARD / ID_INPUT_MOUSE), and udev_device_new_from_devnum
//     round-trips to the same syspath;
//   * the libseat shim opens BOTH /dev/input/event0 and /dev/input/event1 for
//     cosmic-comp (pid 26) with errno 0;
//   * the kernel's [EVSTAT] census shows pid 26 issuing ~20 ioctls per node at
//     startup with ZERO ENOTTY, then, under injection, 234 read() calls that
//     hand out every one of the 476 queued events with 0 ring drops.
// So libinput finds the devices, configures them, and reads their events. Yet
// cosmic-comp's page-flip rate under 476 injected events is identical to its
// idle rate, and its calloop callback calls schedule_render() for EVERY event
// it is handed — so that callback never ran, so `libinput_get_event()` returned
// nothing. The events are swallowed between `read()` and `libinput_get_event()`.
//
// libinput will say why, but only if asked: its default log priority is ERROR
// (libinput.c:1878) and every diagnostic on that path — "not tagged as
// supported input device", "skip unconfigured", "not using input device" — is
// INFO or DEBUG, and neither smithay nor input.rs ever raises it. This probe
// raises it to DEBUG and installs a handler, which is the one thing no shipped
// component does.
//
// It also runs a RAW pre-phase first: it reads /dev/input/event1 directly for a
// few seconds and prints the actual records, with each record's own timestamp
// next to clock_gettime(CLOCK_MONOTONIC) read at the same instant. libinput
// keys frame assembly on EV_SYN/SYN_REPORT and compares event times against its
// own monotonic clock, so a missing SYN or a skewed stamp are both candidate
// explanations and both are invisible to a test that only counts bytes.
//
// The raw phase runs BEFORE the libinput context is created and closes its fd
// before the context opens the node. This is not tidiness: LeandrOS evdev keeps
// ONE ring per device, not one client queue per open, so two readers steal each
// other's events and an overlapping raw phase would corrupt the measurement it
// exists to support.

use std::ffi::{c_char, c_int, c_void, CStr, CString};

// ---------------------------------------------------------------- libinput --

#[repr(C)]
struct LibinputInterface {
    open_restricted:
        unsafe extern "C" fn(*const c_char, c_int, *mut c_void) -> c_int,
    close_restricted: unsafe extern "C" fn(c_int, *mut c_void),
}

// enum libinput_event_type
const EV_DEVICE_ADDED: c_int = 1;
const EV_DEVICE_REMOVED: c_int = 2;
const EV_KEYBOARD_KEY: c_int = 300;
const EV_POINTER_MOTION: c_int = 400;
const EV_POINTER_MOTION_ABSOLUTE: c_int = 401;
const EV_POINTER_BUTTON: c_int = 402;

// enum libinput_device_capability
const CAP_KEYBOARD: c_int = 0;
const CAP_POINTER: c_int = 1;
const CAP_TOUCH: c_int = 2;
const CAP_TABLET_TOOL: c_int = 3;

// enum libinput_log_priority
const LOG_DEBUG: c_int = 10;

extern "C" {
    // libudev (our shim)
    fn udev_new() -> *mut c_void;

    // libinput
    fn libinput_udev_create_context(
        iface: *const LibinputInterface,
        user_data: *mut c_void,
        udev: *mut c_void,
    ) -> *mut c_void;
    fn libinput_udev_assign_seat(li: *mut c_void, seat: *const c_char) -> c_int;
    fn libinput_log_set_priority(li: *mut c_void, prio: c_int);
    fn libinput_log_set_handler(
        li: *mut c_void,
        handler: unsafe extern "C" fn(*mut c_void, c_int, *const c_char, *mut c_void),
    );
    fn libinput_get_fd(li: *mut c_void) -> c_int;
    fn libinput_dispatch(li: *mut c_void) -> c_int;
    fn libinput_get_event(li: *mut c_void) -> *mut c_void;
    fn libinput_event_get_type(ev: *mut c_void) -> c_int;
    fn libinput_event_get_device(ev: *mut c_void) -> *mut c_void;
    fn libinput_event_destroy(ev: *mut c_void);
    fn libinput_device_get_name(dev: *mut c_void) -> *const c_char;
    fn libinput_device_get_sysname(dev: *mut c_void) -> *const c_char;
    fn libinput_device_has_capability(dev: *mut c_void, cap: c_int) -> c_int;
    fn libinput_event_get_pointer_event(ev: *mut c_void) -> *mut c_void;
    fn libinput_event_pointer_get_absolute_x(pe: *mut c_void) -> f64;
    fn libinput_event_pointer_get_absolute_y(pe: *mut c_void) -> f64;
    fn libinput_event_pointer_get_time_usec(pe: *mut c_void) -> u64;
    fn libinput_event_get_keyboard_event(ev: *mut c_void) -> *mut c_void;
    fn libinput_event_keyboard_get_key(ke: *mut c_void) -> u32;
    fn libinput_event_keyboard_get_key_state(ke: *mut c_void) -> c_int;

    // Forward libinput's va_list into a buffer rather than onto a second
    // stdio stream: everything this probe prints must land on ONE stream, or
    // the log line explaining a dropped event ends up separated from the
    // census line it explains by whatever the two buffers felt like doing.
    // Declaring the va_list as an opaque pointer is exactly what a C forwarder
    // does — on both x86_64 SysV and AArch64 AAPCS a va_list parameter is
    // passed as a pointer to its state, so this is ABI-correct on both targets.
    fn vsnprintf(buf: *mut c_char, n: usize, fmt: *const c_char, ap: *mut c_void) -> c_int;
}

const TAG: &str = "[LIP]";

unsafe extern "C" fn open_restricted(
    path: *const c_char,
    flags: c_int,
    _u: *mut c_void,
) -> c_int {
    // Deliberately NOT through libseat: the M13 run already proved the libseat
    // shim opens both nodes with errno 0, so keeping it out of this path
    // removes a component from the thing under test rather than re-testing it.
    let fd = libc::open(path, flags);
    if fd < 0 {
        let e = *libc::__errno_location();
        println!(
            "{TAG} open_restricted path={} flags=0x{:x} FAILED errno={}",
            CStr::from_ptr(path).to_string_lossy(),
            flags,
            e
        );
        return -e;
    }
    println!(
        "{TAG} open_restricted path={} flags=0x{:x} -> fd={}",
        CStr::from_ptr(path).to_string_lossy(),
        flags,
        fd
    );
    fd
}

unsafe extern "C" fn close_restricted(fd: c_int, _u: *mut c_void) {
    println!("{TAG} close_restricted fd={fd}");
    libc::close(fd);
}

unsafe extern "C" fn log_handler(
    _li: *mut c_void,
    prio: c_int,
    fmt: *const c_char,
    ap: *mut c_void,
) {
    let p = match prio {
        10 => "debug",
        20 => "info",
        30 => "error",
        _ => "?",
    };
    let mut buf = [0u8; 1024];
    let n = vsnprintf(buf.as_mut_ptr() as *mut c_char, buf.len(), fmt, ap);
    let msg = if n < 0 {
        "<vsnprintf failed>".to_string()
    } else {
        CStr::from_ptr(buf.as_ptr() as *const c_char)
            .to_string_lossy()
            .into_owned()
    };
    print!("{TAG} libinput {p}: {msg}");
    flush();
}

fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn now_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000
}

// ------------------------------------------------------------- raw pre-phase --

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputEvent {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

fn raw_phase(secs: u64, max_records: usize) {
    let path = CString::new("/dev/input/event1").unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    if fd < 0 {
        println!("{TAG} RAW open FAILED errno={}", unsafe {
            *libc::__errno_location()
        });
        return;
    }
    println!("{TAG} RAW begin fd={fd} secs={secs} sizeof_input_event={}",
             core::mem::size_of::<InputEvent>());

    let end = now_us() + secs * 1_000_000;
    let mut buf = [InputEvent::default(); 64];
    let mut printed = 0usize;
    let mut total = 0usize;
    let mut syns = 0usize;
    let mut last_stamp: u64 = 0;
    let mut non_monotonic = 0usize;
    let mut max_skew_us: i64 = i64::MIN;
    let mut min_skew_us: i64 = i64::MAX;

    while now_us() < end {
        let n = unsafe {
            libc::read(
                fd,
                buf.as_mut_ptr() as *mut c_void,
                core::mem::size_of_val(&buf),
            )
        };
        if n <= 0 {
            unsafe { libc::usleep(5_000) };
            continue;
        }
        let clk = now_us();
        let cnt = (n as usize) / core::mem::size_of::<InputEvent>();
        for e in buf.iter().take(cnt) {
            total += 1;
            let stamp = (e.tv_sec as u64) * 1_000_000 + (e.tv_usec as u64);
            if stamp < last_stamp {
                non_monotonic += 1;
            }
            last_stamp = stamp;
            let skew = clk as i64 - stamp as i64;
            max_skew_us = max_skew_us.max(skew);
            min_skew_us = min_skew_us.min(skew);
            if e.type_ == 0 && e.code == 0 {
                syns += 1;
            }
            if printed < max_records {
                printed += 1;
                println!(
                    "{TAG} RAW rec type={} code={} value={} stamp_us={} clock_us={} skew_us={}",
                    e.type_, e.code, e.value, stamp, clk, skew
                );
            }
        }
    }
    unsafe { libc::close(fd) };
    println!(
        "{TAG} RAW end total={total} syn_report={syns} non_monotonic={non_monotonic} \
         skew_us_min={} skew_us_max={}",
        if min_skew_us == i64::MAX { 0 } else { min_skew_us },
        if max_skew_us == i64::MIN { 0 } else { max_skew_us }
    );
    println!(
        "{TAG} RAW verdict frames={} events_per_frame={}",
        syns,
        if syns > 0 { total as f64 / syns as f64 } else { 0.0 }
    );
}

// ------------------------------------------------------------------- main ----

fn drain(li: *mut c_void, counts: &mut [u64; 8]) {
    loop {
        let ev = unsafe { libinput_get_event(li) };
        if ev.is_null() {
            return;
        }
        let t = unsafe { libinput_event_get_type(ev) };
        match t {
            EV_DEVICE_ADDED | EV_DEVICE_REMOVED => {
                let d = unsafe { libinput_event_get_device(ev) };
                let name = unsafe { CStr::from_ptr(libinput_device_get_name(d)) }
                    .to_string_lossy()
                    .into_owned();
                let sys = unsafe { CStr::from_ptr(libinput_device_get_sysname(d)) }
                    .to_string_lossy()
                    .into_owned();
                let cap = |c| unsafe { libinput_device_has_capability(d, c) };
                println!(
                    "{TAG} EVENT {} sysname={sys} name=\"{name}\" \
                     cap_kbd={} cap_ptr={} cap_touch={} cap_tabtool={}",
                    if t == EV_DEVICE_ADDED { "DEVICE_ADDED" } else { "DEVICE_REMOVED" },
                    cap(CAP_KEYBOARD), cap(CAP_POINTER),
                    cap(CAP_TOUCH), cap(CAP_TABLET_TOOL)
                );
                counts[if t == EV_DEVICE_ADDED { 0 } else { 1 }] += 1;
            }
            EV_POINTER_MOTION_ABSOLUTE | EV_POINTER_MOTION => {
                let pe = unsafe { libinput_event_get_pointer_event(ev) };
                let i = if t == EV_POINTER_MOTION_ABSOLUTE { 2 } else { 3 };
                counts[i] += 1;
                if counts[i] <= 8 {
                    println!(
                        "{TAG} EVENT {} abs_x={:.1} abs_y={:.1} t_us={} clock_us={}",
                        if i == 2 { "POINTER_MOTION_ABSOLUTE" } else { "POINTER_MOTION" },
                        unsafe { libinput_event_pointer_get_absolute_x(pe) },
                        unsafe { libinput_event_pointer_get_absolute_y(pe) },
                        unsafe { libinput_event_pointer_get_time_usec(pe) },
                        now_us()
                    );
                }
            }
            EV_POINTER_BUTTON => {
                counts[4] += 1;
                if counts[4] <= 8 {
                    println!("{TAG} EVENT POINTER_BUTTON");
                }
            }
            EV_KEYBOARD_KEY => {
                let ke = unsafe { libinput_event_get_keyboard_event(ev) };
                counts[5] += 1;
                if counts[5] <= 12 {
                    println!(
                        "{TAG} EVENT KEYBOARD_KEY key={} state={}",
                        unsafe { libinput_event_keyboard_get_key(ke) },
                        unsafe { libinput_event_keyboard_get_key_state(ke) }
                    );
                }
            }
            _ => {
                counts[6] += 1;
                if counts[6] <= 12 {
                    println!("{TAG} EVENT other type={t}");
                }
            }
        }
        unsafe { libinput_event_destroy(ev) };
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, d: u64| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let raw_secs = arg(1, 6);
    let li_secs = arg(2, 45);
    let seat = args.get(3).cloned().unwrap_or_else(|| "seat0".into());

    println!(
        "{TAG} BEGIN pid={} raw_secs={raw_secs} li_secs={li_secs} seat={seat}",
        std::process::id()
    );

    raw_phase(raw_secs, 24);

    let iface = LibinputInterface { open_restricted, close_restricted };
    let udev = unsafe { udev_new() };
    println!("{TAG} udev_new -> {:?}", udev);
    if udev.is_null() {
        println!("{TAG} FATAL udev_new returned NULL");
        return;
    }

    let li = unsafe {
        libinput_udev_create_context(&iface as *const _, core::ptr::null_mut(), udev)
    };
    println!("{TAG} libinput_udev_create_context -> {:?}", li);
    if li.is_null() {
        println!("{TAG} FATAL libinput context is NULL");
        return;
    }

    // The whole point: libinput's own explanation of what it did with each
    // device is DEBUG/INFO and is discarded at its default ERROR priority.
    unsafe {
        libinput_log_set_handler(li, log_handler);
        libinput_log_set_priority(li, LOG_DEBUG);
    }
    println!("{TAG} log priority set to DEBUG");

    let cseat = CString::new(seat.clone()).unwrap();
    let rc = unsafe { libinput_udev_assign_seat(li, cseat.as_ptr()) };
    println!("{TAG} libinput_udev_assign_seat({seat}) -> rc={rc}");

    let fd = unsafe { libinput_get_fd(li) };
    println!("{TAG} libinput_get_fd -> {fd}");

    let mut counts = [0u64; 8];

    // Drain once BEFORE any polling: DEVICE_ADDED is queued by assign_seat and
    // is not backed by any fd, so a probe that waits for readiness first would
    // report zero devices on a perfectly healthy stack.
    let d0 = unsafe { libinput_dispatch(li) };
    println!("{TAG} initial dispatch rc={d0}");
    drain(li, &mut counts);
    println!(
        "{TAG} after assign_seat: device_added={} device_removed={}",
        counts[0], counts[1]
    );

    let end = now_us() + li_secs * 1_000_000;
    let mut polls = 0u64;
    let mut poll_ready = 0u64;
    let mut dispatch_err = 0u64;
    while now_us() < end {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, 200) };
        polls += 1;
        if r > 0 && (pfd.revents & libc::POLLIN) != 0 {
            poll_ready += 1;
        }
        // Dispatch unconditionally, whatever poll said. If poll on libinput's
        // epoll fd is broken (a nested-epoll gap would look exactly like that),
        // this keeps the event measurement alive AND makes the gap legible as
        // `poll_ready=0` next to a nonzero event count, instead of blinding the
        // probe to everything downstream.
        let rc = unsafe { libinput_dispatch(li) };
        if rc != 0 {
            dispatch_err += 1;
            if dispatch_err <= 5 {
                println!("{TAG} libinput_dispatch rc={rc}");
            }
        }
        drain(li, &mut counts);
    }

    println!(
        "{TAG} CENSUS polls={polls} poll_ready={poll_ready} dispatch_err={dispatch_err} \
         device_added={} device_removed={} motion_abs={} motion_rel={} button={} key={} other={}",
        counts[0], counts[1], counts[2], counts[3], counts[4], counts[5], counts[6]
    );
    println!("{TAG} END");
    flush();
}
