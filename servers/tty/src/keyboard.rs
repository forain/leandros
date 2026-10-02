//! The VT keyboard: Linux's `drivers/tty/vt/keyboard.c`, for the text consoles.
//!
//! # What this is
//!
//! A key on a text VT is turned into bytes the way Linux does it, not by a
//! hard-coded US table: the keycode indexes a **keymap** (one table per
//! modifier combination), the entry is a **keysym** (`KTYP << 8 | KVAL`, the
//! encoding of `<linux/keyboard.h>`), and a handler per keysym type produces
//! the bytes — Latin-1/Unicode characters (UTF-8 in `K_UNICODE`), function-key
//! strings, cursor and keypad sequences, modifiers, locks, dead keys, console
//! switches, Alt+keypad composition. The tables start out as Linux's own
//! `defkeymap` ([`crate::defkeymap`], generated from kbd's `loadkeys
//! --mktable`), and `KDGKBENT`/`KDSKBENT`/`KDGKBSENT`/`KDSKBSENT` read and
//! replace them, which is all `loadkeys`/`loadkmap` need. `/bin/loadkmap`
//! applies a binary keymap (kbd's `loadkeys -b` / busybox `loadkmap` format);
//! init runs it at boot from `/etc/vconsole.conf` `KEYMAP=`.
//!
//! # Deliberate limits (all Linux-compatible at the ioctl level)
//!
//! * 16 keymaps (every combination of Shift/AltGr/Ctrl/Alt) rather than 256.
//!   `KDSKBENT` on a table >= 16 is `ENOMEM`, which is what Linux answers when
//!   it runs out of keymaps. Every kbd keymap in common use stays inside 0..15
//!   (`keymaps 0-2,4-6,8-9,12` and friends); ShiftL/ShiftR/CtrlL/CtrlR maps are
//!   the ones that would not fit.
//! * Function-key strings: 64 slots of up to 63 bytes.
//! * The accent (dead-key) table is the default one; `KDSKBDIACR` is not
//!   implemented.
//! * No keyboard LEDs on the device: lock *state* is kept and reported by
//!   `KDGETLED`/`KDGKBLED`, but nothing is sent to virtio-input's status queue.
//! * NumLock starts ON (Linux's compiled default is off). QEMU sends keypad
//!   codes whatever the host LED says, and the console has always typed digits
//!   on the keypad; `setleds -num` semantics are one `KDSKBLED` away.
//!
//! # Contexts
//!
//! Keys for VT 2..6 are translated in the input IRQ ([`crate::vt::kbd_event`]),
//! exactly where Linux translates them, so modifier state is the state at the
//! time of the key. VT 1's keys are translated in task context by the console
//! reader, in queue order, with a modifier state of its own (see
//! [`console_key`]). All shared state sits behind one spinlock that every
//! taker holds with interrupts masked.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use spin::Mutex;

use crate::defkeymap::{DEFAULT_ACCENTS, DEFAULT_FUNC, DEFAULT_KEYMAP};

extern "C" {
    fn arch_interrupt_save() -> usize;
    fn arch_interrupt_restore(f: usize);
}

/// Run `f` with interrupts masked: the keyboard state is shared with the input
/// IRQ, and a task holding the lock when that IRQ arrives on the same CPU would
/// spin forever.
fn irq_off<R>(f: impl FnOnce() -> R) -> R {
    let s = unsafe { arch_interrupt_save() };
    let r = f();
    unsafe { arch_interrupt_restore(s) };
    r
}

// ── <linux/keyboard.h> ────────────────────────────────────────────────────────

pub const NR_KEYS: usize = 256;
/// Keymaps kept. Linux has `MAX_NR_KEYMAPS` = 256; see the module docs.
pub const NR_MAPS: usize = 16;

const KT_LATIN: u16 = 0;
const KT_FN: u16 = 1;
const KT_SPEC: u16 = 2;
const KT_PAD: u16 = 3;
const KT_DEAD: u16 = 4;
const KT_CONS: u16 = 5;
const KT_CUR: u16 = 6;
const KT_SHIFT: u16 = 7;
const KT_META: u16 = 8;
const KT_ASCII: u16 = 9;
const KT_LOCK: u16 = 10;
const KT_LETTER: u16 = 11;
const KT_SLOCK: u16 = 12;
const KT_DEAD2: u16 = 13;
const KT_BRL: u16 = 14;
const NR_TYPES: u16 = 15;

pub const K_HOLE: u16 = 0x0200;
pub const K_NOSUCHMAP: u16 = 0x027f;

const KG_SHIFT: u8 = 0;
const NR_SHIFT: usize = 9;
/// `K_CAPSSHIFT`'s KVAL: acts like Shift but clears CapsLock.
const KVAL_CAPSSHIFT: u8 = 8;

/// Highest valid KVAL per type, as `max_vals[]` in keyboard.c.
const MAX_VALS: [u8; NR_TYPES as usize] = [255, 255, 19, 19, 26, 255, 3, 8, 255, 25, 7, 255, 7, 255, 8];

/// `ret_diacr[]`: the character each `KT_DEAD` value stands for.
const RET_DIACR: [u8; 27] = *b"`'^~\",_U.*=cki#o!?+-)(:n;$@";

/// `KDSETLED`/`KDGETLED` bits, which are also the `VC_*LOCK` flag bits.
pub const LED_SCR: u8 = 0x01;
pub const LED_NUM: u8 = 0x02;
pub const LED_CAP: u8 = 0x04;

/// `KDGKBMETA` values.
pub const K_METABIT: u32 = 0x03;
pub const K_ESCPREFIX: u32 = 0x04;

// Per-VT mode flags (Linux's VC_* kbd modes that are not the kbd mode itself).
const MF_APPLIC: u8 = 1 << 0; // DECKPAM: keypad application mode
const MF_CKMODE: u8 = 1 << 1; // DECCKM: cursor keys send ESC O x
const MF_CRLF: u8 = 1 << 2; // LNM: Enter sends CR LF
const MF_METABIT: u8 = 1 << 3; // set = K_METABIT; clear = K_ESCPREFIX (the default)

// ── Tables ────────────────────────────────────────────────────────────────────

const MAX_FUNC: usize = 64;
const FUNC_LEN: usize = 64;

struct Tables {
    map: [[u16; NR_KEYS]; NR_MAPS],
    /// Bit `m` set when keymap `m` exists. A key in a missing map produces
    /// nothing, as on Linux.
    allocated: u32,
    func: [[u8; FUNC_LEN]; MAX_FUNC],
    func_len: [u8; MAX_FUNC],
}

const fn default_tables() -> Tables {
    let mut func = [[0u8; FUNC_LEN]; MAX_FUNC];
    let mut func_len = [0u8; MAX_FUNC];
    let mut i = 0;
    while i < DEFAULT_FUNC.len() {
        let s = DEFAULT_FUNC[i];
        let mut j = 0;
        while j < s.len() {
            func[i][j] = s[j];
            j += 1;
        }
        func_len[i] = s.len() as u8;
        i += 1;
    }
    Tables { map: DEFAULT_KEYMAP, allocated: (1 << NR_MAPS) - 1, func, func_len }
}

static TABLES: Mutex<Tables> = Mutex::new(default_tables());

// ── State ─────────────────────────────────────────────────────────────────────

/// Physically held modifiers: Linux's `shift_down[]`/`shift_state`, plus the
/// key-down bitmap `compute_shiftstate` rebuilds them from.
#[derive(Clone, Copy)]
pub struct Shift {
    down: [u8; NR_SHIFT],
    state: u8,
    keys: [u64; NR_KEYS / 64],
}

impl Shift {
    pub const fn new() -> Self { Self { down: [0; NR_SHIFT], state: 0, keys: [0; NR_KEYS / 64] } }
}

/// Per-VT keyboard state (Linux's `struct kbd_struct`, minus the mode, which
/// lives with the VT in `vt.rs`).
#[derive(Clone, Copy)]
struct VtKbd {
    ledflag: u8,
    default_ledflag: u8,
    /// `KDSETLED` override: `Some(leds)` shows those instead of the flags.
    led_ioctl: Option<u8>,
    lockstate: u8,
    slockstate: u8,
    modeflags: u8,
    /// Pending dead key (a code point), 0 = none.
    diacr: u32,
    dead_key_next: bool,
    /// Alt+keypad composition in progress.
    npadch: Option<u32>,
}

const VT_KBD_INIT: VtKbd = VtKbd {
    ledflag: LED_NUM,
    default_ledflag: LED_NUM,
    led_ioctl: None,
    lockstate: 0,
    slockstate: 0,
    modeflags: 0,
    diacr: 0,
    dead_key_next: false,
    npadch: None,
};

struct State {
    vt: [VtKbd; crate::vt::VT_COUNT],
    /// The IRQ-side modifier state: every keyboard edge, whatever is on screen.
    irq: Shift,
    /// VT 1's reader-side modifier state; see [`console_key`].
    console: Shift,
    /// [`RESYNC`] value `console` was last synchronised at.
    console_gen: u32,
}

static STATE: Mutex<State> = Mutex::new(State {
    vt: [VT_KBD_INIT; crate::vt::VT_COUNT],
    irq: Shift::new(),
    console: Shift::new(),
    console_gen: 0,
});

/// Bumped whenever VT 1 regains the keyboard (a switch to it, `KD_TEXT` on
/// it). The console reader's modifier state is rebuilt from the IRQ-side one
/// at the next key, because releases that happened while the keyboard was
/// elsewhere never reached its queue.
static RESYNC: AtomicU32 = AtomicU32::new(0);

pub fn console_resync() { RESYNC.fetch_add(1, Ordering::Relaxed); }

// ── Output ────────────────────────────────────────────────────────────────────

/// What one key produced.
pub struct Out {
    pub buf: [u8; 96],
    pub n: usize,
    /// A console switch the key asked for (1-based VT), 0 = none.
    pub switch_to: usize,
    /// Relative switch: -1 previous, +1 next, 2 last console.
    pub switch_rel: i8,
}

impl Out {
    pub const fn new() -> Self { Self { buf: [0; 96], n: 0, switch_to: 0, switch_rel: 0 } }
    pub fn bytes(&self) -> &[u8] { &self.buf[..self.n] }
    fn put(&mut self, b: u8) {
        if self.n < self.buf.len() {
            self.buf[self.n] = b;
            self.n += 1;
        }
    }
    fn puts(&mut self, s: &[u8]) { for &b in s { self.put(b); } }
    fn put_utf8(&mut self, c: u32) {
        if c < 0x80 {
            self.put(c as u8);
        } else if c < 0x800 {
            self.put(0xc0 | (c >> 6) as u8);
            self.put(0x80 | (c & 0x3f) as u8);
        } else if c < 0x10000 {
            if (0xd800..0xe000).contains(&c) || c == 0xffff { return; }
            self.put(0xe0 | (c >> 12) as u8);
            self.put(0x80 | ((c >> 6) & 0x3f) as u8);
            self.put(0x80 | (c & 0x3f) as u8);
        } else if c < 0x110000 {
            self.put(0xf0 | (c >> 18) as u8);
            self.put(0x80 | ((c >> 12) & 0x3f) as u8);
            self.put(0x80 | ((c >> 6) & 0x3f) as u8);
            self.put(0x80 | (c & 0x3f) as u8);
        }
    }
}

// ── Translation ───────────────────────────────────────────────────────────────

/// One key event against one VT. `route` false means the key belongs to some
/// other owner (a graphical session, a raw-mode client, VT 1 while this is the
/// IRQ side): only the modifier bookkeeping runs, so a Shift held across a
/// switch is still held after it, and no lock or dead-key state of any VT moves.
struct Ctx<'a> {
    t: &'a Tables,
    vt: &'a mut VtKbd,
    sh: &'a mut Shift,
    unicode: bool,
    rep: bool,
    out: &'a mut Out,
}

fn allocated(t: &Tables, m: u8) -> bool { (m as usize) < NR_MAPS && t.allocated & (1 << m) != 0 }

fn process(t: &Tables, vt: &mut VtKbd, sh: &mut Shift, code: u16, value: i32, unicode: bool,
           route: bool, out: &mut Out) {
    let code = code as usize;
    if code >= NR_KEYS { return; }
    let down = value != 0;
    let rep = value == 2;
    if down { sh.keys[code / 64] |= 1 << (code % 64); } else { sh.keys[code / 64] &= !(1 << (code % 64)); }

    let shift_final = if route { (sh.state | vt.slockstate) ^ vt.lockstate } else { sh.state };
    if !allocated(t, shift_final) {
        // Linux: compute_shiftstate() and drop the key. Rebuilding from the
        // held keys is what stops a modifier released in a missing map from
        // staying down forever.
        compute_shiftstate(t, sh);
        if route { vt.slockstate = 0; }
        return;
    }
    let mut sym = t.map[shift_final as usize][code];
    let mut typ = sym >> 8;
    if !route {
        if typ == KT_SHIFT || typ == KT_SLOCK { k_shift(sh, (sym & 0xff) as u8, !down, rep); }
        return;
    }
    if typ >= NR_TYPES {
        // A Unicode keymap entry (only loadable in K_UNICODE).
        if down {
            let mut c = Ctx { t, vt, sh, unicode, rep, out };
            k_unicode(&mut c, (sym ^ 0xf000) as u32);
        }
        return;
    }
    if typ == KT_LETTER {
        typ = KT_LATIN;
        if vt.ledflag & LED_CAP != 0 {
            let m2 = shift_final ^ (1 << KG_SHIFT);
            if allocated(t, m2) { sym = t.map[m2 as usize][code]; }
        }
    }
    let val = (sym & 0xff) as u8;
    let up = !down;
    let mut c = Ctx { t, vt, sh, unicode, rep, out };
    match typ {
        KT_LATIN => k_self(&mut c, val, up),
        KT_FN => if !up { k_fn(&mut c, val) },
        KT_SPEC => if !up { k_spec(&mut c, val) },
        KT_PAD => if !up { k_pad(&mut c, val) },
        KT_DEAD => if !up { k_dead(&mut c, *RET_DIACR.get(val as usize).unwrap_or(&b' ') as u32) },
        KT_CONS => if !up { c.out.switch_to = val as usize + 1 },
        KT_CUR => if !up { k_cur(&mut c, val) },
        KT_SHIFT => {
            let old = c.sh.state;
            k_shift(c.sh, val, up, rep);
            // Releasing the modifier that was held for Alt+keypad digits
            // types the composed character.
            if up && c.sh.state != old {
                if let Some(v) = c.vt.npadch.take() { put_uni(&mut c, v); }
            }
        }
        KT_META => if !up {
            if c.vt.modeflags & MF_METABIT != 0 { c.out.put(val | 0x80); } else { c.out.put(0x1b); c.out.put(val); }
        },
        KT_ASCII => if !up {
            let (base, v) = if val < 10 { (10u32, val as u32) } else { (16u32, val as u32 - 10) };
            let cur = c.vt.npadch.unwrap_or(0);
            c.vt.npadch = Some(cur.wrapping_mul(base).wrapping_add(v));
        },
        KT_LOCK => if !up && !rep { c.vt.lockstate ^= 1 << (val & 7) },
        KT_SLOCK => {
            k_shift(c.sh, val, up, rep);
            if !up && !rep {
                c.vt.slockstate ^= 1 << (val & 7);
                if !allocated(c.t, c.vt.lockstate ^ c.vt.slockstate) { c.vt.slockstate = 0; }
            }
        }
        KT_DEAD2 => if !up { k_dead(&mut c, val as u32) },
        KT_BRL | _ => {}
    }
    if typ != KT_SLOCK { vt.slockstate = 0; }
}

/// Rebuild `shift_down`/`shift_state` from the keys still held, reading their
/// meaning from the plain map — `compute_shiftstate()`.
fn compute_shiftstate(t: &Tables, sh: &mut Shift) {
    sh.down = [0; NR_SHIFT];
    sh.state = 0;
    for w in 0..sh.keys.len() {
        let mut bits = sh.keys[w];
        while bits != 0 {
            let b = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let sym = t.map[0][w * 64 + b];
            let typ = sym >> 8;
            if typ != KT_SHIFT && typ != KT_SLOCK { continue; }
            let mut v = (sym & 0xff) as usize;
            if v == KVAL_CAPSSHIFT as usize { v = KG_SHIFT as usize; }
            if v < NR_SHIFT {
                sh.down[v] = sh.down[v].saturating_add(1);
                sh.state |= 1 << v;
            }
        }
    }
}

fn k_shift(sh: &mut Shift, val: u8, up: bool, rep: bool) {
    if rep { return; }
    let mut v = val as usize;
    if v == KVAL_CAPSSHIFT as usize { v = KG_SHIFT as usize; }
    if v >= NR_SHIFT { return; }
    if up {
        if sh.down[v] > 0 { sh.down[v] -= 1; }
    } else {
        sh.down[v] = sh.down[v].saturating_add(1);
    }
    if sh.down[v] > 0 { sh.state |= 1 << v; } else { sh.state &= !(1 << v); }
}

/// A Latin-1 keysym: `k_self` = `k_unicode(conv_8bit_to_uni(value))`, with
/// the identity user map (Latin-1 is Unicode's first 256 code points).
fn k_self(c: &mut Ctx, val: u8, up: bool) {
    if up { return; }
    k_unicode(c, val as u32);
}

fn k_unicode(c: &mut Ctx, mut v: u32) {
    if c.vt.diacr != 0 { v = handle_diacr(c, v); }
    if c.vt.dead_key_next {
        c.vt.dead_key_next = false;
        c.vt.diacr = v;
        return;
    }
    put_uni(c, v);
}

/// Emit one character in the VT's keyboard mode: UTF-8 in `K_UNICODE`, the
/// Latin-1 byte in `K_XLATE` (nothing for a character outside Latin-1, which
/// is `conv_uni_to_8bit` failing).
fn put_uni(c: &mut Ctx, v: u32) {
    if c.unicode {
        c.out.put_utf8(v);
    } else if v < 0x100 {
        c.out.put(v as u8);
    }
}

fn k_dead(c: &mut Ctx, ch: u32) {
    c.vt.diacr = if c.vt.diacr != 0 { handle_diacr(c, ch) } else { ch };
}

/// `handle_diacr`: compose the pending dead key with `ch`, or emit the dead
/// key's own character and return `ch` unchanged.
fn handle_diacr(c: &mut Ctx, ch: u32) -> u32 {
    let d = c.vt.diacr;
    c.vt.diacr = 0;
    for &(dia, base, res) in DEFAULT_ACCENTS.iter() {
        if dia as u32 == d && base as u32 == ch { return res as u32; }
    }
    if ch == b' ' as u32 || ch == d { return d; }
    put_uni(c, d);
    ch
}

fn k_fn(c: &mut Ctx, val: u8) {
    let i = val as usize;
    if i >= MAX_FUNC { return; }
    let n = c.t.func_len[i] as usize;
    let s = c.t.func[i];
    c.out.puts(&s[..n]);
}

/// `applkey`: `ESC [ k`, or `ESC O k` in application mode.
fn applkey(c: &mut Ctx, key: u8, app: bool) {
    c.out.put(0x1b);
    c.out.put(if app { b'O' } else { b'[' });
    c.out.put(key);
}

fn k_cur(c: &mut Ctx, val: u8) {
    const CUR: &[u8; 4] = b"BDCA";
    if (val as usize) < CUR.len() {
        let app = c.vt.modeflags & MF_CKMODE != 0;
        applkey(c, CUR[val as usize], app);
    }
}

fn k_pad(c: &mut Ctx, val: u8) {
    const PAD: &[u8; 20] = b"0123456789+-*/\r,.?()";
    const APP: &[u8; 20] = b"pqrstuvwxylSRQMnnmPQ";
    let v = val as usize;
    if v >= PAD.len() { return; }
    if c.vt.modeflags & MF_APPLIC != 0 && c.sh.down[KG_SHIFT as usize] == 0 {
        applkey(c, APP[v], true);
        return;
    }
    if c.vt.ledflag & LED_NUM == 0 {
        // KVAL(K_PCOMMA)=15, KVAL(K_PDOT)=16; K_P0..K_P9 = 0..9. The KT_FN
        // values are K_INSERT 21, K_REMOVE 22, K_SELECT 23, K_PGUP 24,
        // K_PGDN 25, K_FIND 20.
        match v {
            15 | 16 => return k_fn(c, 22),
            0 => return k_fn(c, 21),
            1 => return k_fn(c, 23),
            2 => return k_cur(c, 0),
            3 => return k_fn(c, 25),
            4 => return k_cur(c, 1),
            5 => { let app = c.vt.modeflags & MF_APPLIC != 0; return applkey(c, b'G', app); }
            6 => return k_cur(c, 2),
            7 => return k_fn(c, 20),
            8 => return k_cur(c, 3),
            9 => return k_fn(c, 24),
            _ => {}
        }
    }
    c.out.put(PAD[v]);
    if v == 14 && c.vt.modeflags & MF_CRLF != 0 { c.out.put(b'\n'); }
}

fn k_spec(c: &mut Ctx, val: u8) {
    match val {
        // K_ENTER: a pending dead key is typed as itself first.
        1 => {
            if c.vt.diacr != 0 {
                let d = c.vt.diacr;
                c.vt.diacr = 0;
                put_uni(c, d);
            }
            c.out.put(b'\r');
            if c.vt.modeflags & MF_CRLF != 0 { c.out.put(b'\n'); }
        }
        6 => c.out.switch_rel = 2,                       // K_CONS: last console
        7 => if !c.rep { c.vt.ledflag ^= LED_CAP },     // K_CAPS
        8 => {                                           // K_NUM
            if c.vt.modeflags & MF_APPLIC != 0 { applkey(c, b'P', true); }
            else if !c.rep { c.vt.ledflag ^= LED_NUM; }
        }
        9 => if !c.rep { c.vt.ledflag ^= LED_SCR },     // K_HOLD (Scroll Lock)
        13 => if !c.rep { c.vt.ledflag |= LED_CAP },    // K_CAPSON
        14 => c.vt.dead_key_next = true,                 // K_COMPOSE
        16 => c.out.switch_rel = -1,                     // K_DECRCONSOLE
        17 => c.out.switch_rel = 1,                      // K_INCRCONSOLE
        19 => if !c.rep { c.vt.ledflag ^= LED_NUM },    // K_BARENUMLOCK
        // K_HOLE, show-regs/mem/state, Break, scroll back/forward, Boot
        // (Ctrl+Alt+Del), SAK, spawn console: nothing to do here.
        _ => {}
    }
}

// ── Entry points ──────────────────────────────────────────────────────────────

/// IRQ side: one keyboard `EV_KEY`. `route_idx` is the zero-based VT that owns
/// the keyboard as a *text* console, or `None` when nobody's line discipline
/// does (bookkeeping only).
pub fn irq_key(route_idx: Option<usize>, code: u16, value: i32, unicode: bool, out: &mut Out) {
    irq_off(|| {
        let t = TABLES.lock();
        let mut s = STATE.lock();
        let s = &mut *s;
        match route_idx {
            Some(i) if i < s.vt.len() => process(&t, &mut s.vt[i], &mut s.irq, code, value, unicode, true, out),
            _ => {
                let mut scratch = VT_KBD_INIT;
                process(&t, &mut scratch, &mut s.irq, code, value, unicode, false, out)
            }
        }
    })
}

/// Task side: VT 1's console reader translating one popped `EV_KEY`.
///
/// The console reader sees keys in queue order, possibly long after they were
/// typed (type-ahead into a busy shell), so it keeps its own modifier state and
/// applies each edge in that order — using the IRQ-side state would capitalise
/// or not according to what is held when the shell gets round to reading. Its
/// queue misses the edges typed while the keyboard belonged to someone else,
/// so whenever VT 1 regains the keyboard ([`console_resync`]) the reader's
/// state is replaced by the IRQ side's, which saw every edge.
pub fn console_key(code: u16, value: i32, unicode: bool, out: &mut Out) {
    irq_off(|| {
        let t = TABLES.lock();
        let mut s = STATE.lock();
        let s = &mut *s;
        let g = RESYNC.load(Ordering::Relaxed);
        if s.console_gen != g {
            s.console_gen = g;
            s.console = s.irq;
        }
        process(&t, &mut s.vt[0], &mut s.console, code, value, unicode, true, out)
    })
}

/// The output side of the VT saw `ESC [ ? 1 h/l` (DECCKM), `ESC =`/`ESC >`
/// (DECKPAM/DECKPNM) or `ESC [ 20 h/l` (LNM).
pub fn set_cursor_app(idx: usize, on: bool) { set_flag(idx, MF_CKMODE, on); }
pub fn set_keypad_app(idx: usize, on: bool) { set_flag(idx, MF_APPLIC, on); }
pub fn set_crlf(idx: usize, on: bool) { set_flag(idx, MF_CRLF, on); }

fn set_flag(idx: usize, f: u8, on: bool) {
    irq_off(|| {
        let mut s = STATE.lock();
        if let Some(v) = s.vt.get_mut(idx) {
            if on { v.modeflags |= f; } else { v.modeflags &= !f; }
        }
    })
}

/// A VT was deallocated or reset: back to the boot defaults.
pub fn reset_vt(idx: usize) {
    irq_off(|| {
        let mut s = STATE.lock();
        if let Some(v) = s.vt.get_mut(idx) { *v = VT_KBD_INIT; }
    })
}

// ── ioctl backends ────────────────────────────────────────────────────────────

const ENOMEM: isize = -12;
const EINVAL: isize = -22;

/// `KDGKBENT`. Returns the keysym in user form.
pub fn get_ent(table: u8, index: u8, unicode: bool) -> u16 {
    irq_off(|| {
        let t = TABLES.lock();
        if !allocated(&t, table) {
            return if index != 0 { K_HOLE } else { K_NOSUCHMAP };
        }
        let v = t.map[table as usize][index as usize];
        // A Unicode entry reads back as a hole outside K_UNICODE, as on Linux.
        if !unicode && (v >> 8) >= NR_TYPES { K_HOLE } else { v }
    })
}

/// `KDSKBENT`.
pub fn set_ent(table: u8, index: u8, value: u16, unicode: bool) -> isize {
    // Index 0 with K_NOSUCHMAP deallocates the map (never map 0).
    if index == 0 && value == K_NOSUCHMAP {
        if table == 0 { return EINVAL; }
        if table as usize >= NR_MAPS { return 0; }
        irq_off(|| TABLES.lock().allocated &= !(1 << table));
        return 0;
    }
    let typ = value >> 8;
    if typ < NR_TYPES {
        if (value & 0xff) as u8 > MAX_VALS[typ as usize] { return EINVAL; }
    } else if !unicode {
        return EINVAL;
    }
    // Entry 0 only checks the arguments (Linux: keycode 0 is never typed).
    if index == 0 { return 0; }
    if table as usize >= NR_MAPS { return ENOMEM; }
    irq_off(|| {
        let mut t = TABLES.lock();
        if t.allocated & (1 << table) == 0 {
            // A freshly allocated map starts as holes.
            t.map[table as usize] = [K_HOLE; NR_KEYS];
            t.allocated |= 1 << table;
        }
        t.map[table as usize][index as usize] = value;
    });
    // A modifier may have moved under held keys.
    irq_off(|| {
        let t = TABLES.lock();
        let mut s = STATE.lock();
        compute_shiftstate(&t, &mut s.irq);
    });
    0
}

/// `KDGKBSENT`: copy function string `func` (NUL-terminated) into `buf`.
pub fn get_func(func: u8, buf: &mut [u8]) -> usize {
    irq_off(|| {
        let t = TABLES.lock();
        let i = func as usize;
        let n = if i < MAX_FUNC { (t.func_len[i] as usize).min(buf.len().saturating_sub(1)) } else { 0 };
        if i < MAX_FUNC { buf[..n].copy_from_slice(&t.func[i][..n]); }
        if n < buf.len() { buf[n] = 0; }
        n
    })
}

/// `KDSKBSENT`.
pub fn set_func(func: u8, s: &[u8]) -> isize {
    let i = func as usize;
    if i >= MAX_FUNC || s.len() >= FUNC_LEN { return ENOMEM; }
    irq_off(|| {
        let mut t = TABLES.lock();
        t.func[i] = [0; FUNC_LEN];
        t.func[i][..s.len()].copy_from_slice(s);
        t.func_len[i] = s.len() as u8;
    });
    0
}

/// `KDGETLED`: the LEDs VT `idx` shows.
pub fn get_led(idx: usize) -> u8 {
    irq_off(|| {
        let s = STATE.lock();
        s.vt.get(idx).map_or(0, |v| v.led_ioctl.unwrap_or(v.ledflag))
    })
}

/// `KDSETLED`: bits 0..2 drive the LEDs directly; anything above returns them
/// to showing the lock flags.
pub fn set_led(idx: usize, arg: usize) {
    irq_off(|| {
        let mut s = STATE.lock();
        if let Some(v) = s.vt.get_mut(idx) {
            v.led_ioctl = if arg & !7 == 0 { Some(arg as u8) } else { None };
        }
    })
}

/// `KDGKBLED`: flags in bits 0..2, defaults in bits 4..6.
pub fn get_kbled(idx: usize) -> u8 {
    irq_off(|| {
        let s = STATE.lock();
        s.vt.get(idx).map_or(0, |v| v.ledflag | (v.default_ledflag << 4))
    })
}

/// `KDSKBLED`.
pub fn set_kbled(idx: usize, arg: usize) -> isize {
    if arg & !0x77 != 0 { return EINVAL; }
    irq_off(|| {
        let mut s = STATE.lock();
        if let Some(v) = s.vt.get_mut(idx) {
            v.ledflag = (arg & 7) as u8;
            v.default_ledflag = ((arg >> 4) & 7) as u8;
        }
    });
    0
}

/// `KDGKBMETA`.
pub fn get_meta(idx: usize) -> u32 {
    irq_off(|| {
        let s = STATE.lock();
        if s.vt.get(idx).map_or(false, |v| v.modeflags & MF_METABIT != 0) { K_METABIT } else { K_ESCPREFIX }
    })
}

/// `KDSKBMETA`.
pub fn set_meta(idx: usize, arg: usize) -> isize {
    let bit = match arg as u32 { K_METABIT => true, K_ESCPREFIX => false, _ => return EINVAL };
    set_flag(idx, MF_METABIT, bit);
    0
}

// ── Autorepeat ────────────────────────────────────────────────────────────────
//
// Linux's input core repeats the most recently pressed key of a device that
// advertises EV_REP — virtio-input's keyboard does — at REP_DELAY 250 ms and
// REP_PERIOD 33 ms by default, and `KDKBDREP` (kbdrate) changes both. Here the
// repeat is generated for the text consoles only: evdev clients (compositors,
// libinput) run their own repeat from the press, exactly as they do on Linux
// where they ignore value-2 events, and value 2 on the keyboard node is
// already this tree's serial-byte marker.

/// Repeat delay and period in milliseconds.
static REP_DELAY_MS: AtomicU32 = AtomicU32::new(250);
static REP_PERIOD_MS: AtomicU32 = AtomicU32::new(33);
/// Key being repeated, plus one; 0 = none.
static REP_KEY: AtomicU32 = AtomicU32::new(0);
/// Monotonic ns of the next repeat.
static REP_NEXT: AtomicU64 = AtomicU64::new(0);
/// Repeats generated (diagnostic, `/proc/vtstat`).
pub static REPEATS: AtomicU64 = AtomicU64::new(0);

/// A key edge reached the keyboard: (re)arm or disarm the repeat.
pub fn repeat_edge(code: u16, value: i32, now_ns: u64) {
    match value {
        1 => {
            let d = REP_DELAY_MS.load(Ordering::Relaxed) as u64;
            REP_NEXT.store(now_ns + d * 1_000_000, Ordering::Relaxed);
            REP_KEY.store(code as u32 + 1, Ordering::Release);
        }
        0 => { let _ = REP_KEY.compare_exchange(code as u32 + 1, 0, Ordering::AcqRel, Ordering::Relaxed); }
        _ => {}
    }
}

/// Stop repeating (a VT switch, a mode change).
pub fn repeat_cancel() { REP_KEY.store(0, Ordering::Release); }

/// Is a repeat due at `now_ns`? Returns the key and advances the deadline.
pub fn repeat_due(now_ns: u64) -> Option<u16> {
    let k = REP_KEY.load(Ordering::Acquire);
    if k == 0 { return None; }
    let next = REP_NEXT.load(Ordering::Relaxed);
    if now_ns < next { return None; }
    let p = REP_PERIOD_MS.load(Ordering::Relaxed) as u64 * 1_000_000;
    if p == 0 { return None; }
    // Keep the cadence (deadline + period) unless the tick fell far behind,
    // so a 10 ms tick still averages the configured rate.
    let mut nn = next + p;
    if nn <= now_ns { nn = now_ns + p; }
    if REP_NEXT.compare_exchange(next, nn, Ordering::AcqRel, Ordering::Relaxed).is_err() { return None; }
    REPEATS.fetch_add(1, Ordering::Relaxed);
    Some((k - 1) as u16)
}

/// `KDKBDREP`: a positive field sets it; the current values are returned.
pub fn kbdrep(delay: i32, period: i32) -> (i32, i32) {
    if delay > 0 { REP_DELAY_MS.store(delay as u32, Ordering::Relaxed); }
    if period > 0 { REP_PERIOD_MS.store(period as u32, Ordering::Relaxed); }
    (REP_DELAY_MS.load(Ordering::Relaxed) as i32, REP_PERIOD_MS.load(Ordering::Relaxed) as i32)
}

pub fn repeat_params() -> (u32, u32) {
    (REP_DELAY_MS.load(Ordering::Relaxed), REP_PERIOD_MS.load(Ordering::Relaxed))
}

