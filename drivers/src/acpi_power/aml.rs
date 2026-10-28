//! A small AML interpreter: loads the DSDT/SSDTs into a namespace and
//! evaluates control methods (`_BST`, `_BIF`, `_PSR`, `_LID`, `_Qxx`, ...).
//!
//! Scope: what laptop power methods use — names, methods, operation regions
//! with Field/IndexField/BankField units, buffer fields, packages, integer and
//! buffer/string arithmetic, If/While, Notify, Mutex/Event (no-ops: one
//! interpreter thread), `_OSI`. Unsupported: Load/LoadTable (ignored),
//! RefOf on locals, GenericSerialBus/GPIO regions (read as 0).
//!
//! Lenient by design: an unknown opcode aborts the current method (or, at load
//! time, the rest of the enclosing Scope/Device body, whose end is known from
//! its PkgLength), never the whole table.
//!
//! Pure `core` + `alloc`: all hardware access goes through [`Host`], so this
//! file also builds and runs on a development host against a dumped DSDT.

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::cmp::Ordering as Ord;

/// Operation region address spaces.
pub const SPACE_MEM: u8 = 0;
pub const SPACE_IO: u8 = 1;
pub const SPACE_PCI: u8 = 2;
pub const SPACE_EC: u8 = 3;

/// Hardware behind the interpreter.
pub trait Host {
    /// Read `width` bits (8/16/32/64) at `addr` in `space`. For SPACE_PCI,
    /// `addr = bus << 32 | dev << 24 | func << 16 | offset`.
    fn read(&mut self, space: u8, addr: u64, width: u8) -> u64;
    fn write(&mut self, space: u8, addr: u64, width: u8, val: u64);
    fn sleep_ms(&mut self, ms: u64);
    fn stall_us(&mut self, us: u64);
    /// 100 ns ticks, monotonic (the `Timer` opcode).
    fn timer(&mut self) -> u64;
    /// `Notify(node, code)`.
    fn notify(&mut self, node: usize, code: u64);
    fn log(&mut self, msg: &str);
}

#[derive(Clone, Debug, Default)]
pub enum V {
    #[default]
    Uninit,
    Int(u64),
    Str(Vec<u8>),
    Buf(Rc<RefCell<Vec<u8>>>),
    Pkg(Rc<RefCell<Vec<V>>>),
    /// A namespace node (device, method, a package element naming an
    /// object, or a reference made by RefOf / CondRefOf).
    Node(usize),
    /// Index() into a buffer.
    BufIdx(Rc<RefCell<Vec<u8>>>, usize),
    /// Index() into a package.
    PkgIdx(Rc<RefCell<Vec<V>>>, usize),
}

impl V {
    pub fn buf(b: Vec<u8>) -> V { V::Buf(Rc::new(RefCell::new(b))) }
    pub fn pkg(p: Vec<V>) -> V { V::Pkg(Rc::new(RefCell::new(p))) }
    /// Deep copy (Store / Name / CopyObject semantics).
    pub fn deep(&self) -> V {
        match self {
            V::Buf(b) => V::buf(b.borrow().clone()),
            V::Pkg(p) => V::pkg(p.borrow().iter().map(|e| e.deep()).collect()),
            o => o.clone(),
        }
    }
    pub fn as_int(&self) -> Option<u64> { if let V::Int(i) = self { Some(*i) } else { None } }
    /// Bytes of a string or buffer, up to the first NUL.
    pub fn as_bytes(&self) -> Option<Vec<u8>> {
        match self {
            V::Str(s) => Some(s.clone()),
            V::Buf(b) => {
                let b = b.borrow();
                let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                Some(b[..n].to_vec())
            }
            _ => None,
        }
    }
    /// Elements of a package (shallow).
    pub fn elements(&self) -> Option<Vec<V>> {
        if let V::Pkg(p) = self { Some(p.borrow().clone()) } else { None }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Err {
    BadOpcode(u8, u8, usize),
    Undefined(usize),
    Type,
    Depth,
    Eof,
    Fatal,
    Loop,
}

#[derive(Clone, Copy)]
enum FieldKind {
    Region(usize),
    Index { index: usize, data: usize },
    Bank { region: usize, bank: usize, val: u64 },
}

#[derive(Clone, Copy)]
struct FieldUnit {
    kind: FieldKind,
    bit_off: u64,
    bits: u64,
    access: u8,
    update: u8,
}

#[derive(Clone)]
enum Obj {
    Scope,
    Device,
    Name(V),
    Method { code: &'static [u8], argc: u8, int32: bool },
    Region { space: u8, base: u64 },
    Field(FieldUnit),
    BufField { buf: Rc<RefCell<Vec<u8>>>, bit_off: usize, bits: usize },
    Mutex,
    Event,
    Processor,
    PowerRes,
    Thermal,
    Alias(usize),
    External { argc: u8 },
    Osi,
}

struct Node {
    seg: [u8; 4],
    parent: usize,
    obj: Obj,
}

pub struct Ns {
    nodes: Vec<Node>,
    kids: BTreeMap<(usize, [u8; 4]), usize>,
    pub errors: u32,
    pub notifies: u32,
    depth: u32,
    int32: bool,
}

#[derive(Clone)]
struct NamePath {
    root: bool,
    up: u8,
    segs: Vec<[u8; 4]>,
}

enum Flow { Next, Ret(V), Break, Cont }

enum Tgt {
    None,
    Local(usize),
    Arg(usize),
    Node(usize),
    Ref(V),
    Debug,
}

struct Frame {
    scope: usize,
    args: [V; 7],
    locals: [V; 8],
}

impl Frame {
    fn new(scope: usize) -> Frame { Frame { scope, args: Default::default(), locals: Default::default() } }
}

type R<T> = Result<T, Err>;

const OSI_TRUE: &[&[u8]] = &[
    b"Windows 2000", b"Windows 2001", b"Windows 2001 SP1", b"Windows 2001.1", b"Windows 2001 SP2",
    b"Windows 2001.1 SP1", b"Windows 2006", b"Windows 2006.1", b"Windows 2006 SP1", b"Windows 2006 SP2",
    b"Windows 2009", b"Windows 2012", b"Windows 2013", b"Windows 2015", b"Windows 2016", b"Windows 2017",
    b"Windows 2017.2", b"Windows 2018", b"Windows 2018.2", b"Windows 2019", b"Windows 2020",
    b"Module Device", b"Processor Device", b"3.0 Thermal Model", b"3.0 _SCP Extensions",
    b"Processor Aggregator Device", b"Extended Address Space Descriptor",
];

fn pkg_len(c: &[u8], p: &mut usize) -> R<usize> {
    let start = *p;
    let lead = *c.get(*p).ok_or(Err::Eof)?;
    *p += 1;
    let n = (lead >> 6) as usize;
    let len = if n == 0 {
        (lead & 0x3F) as usize
    } else {
        let mut v = (lead & 0x0F) as usize;
        for i in 0..n {
            v |= (*c.get(*p).ok_or(Err::Eof)? as usize) << (4 + 8 * i);
            *p += 1;
        }
        v
    };
    let end = start + len;
    if end > c.len() || end < *p { return Err(Err::Eof); }
    Ok(end)
}

/// A PkgLength used as a plain number (field widths): (value, encoded size).
fn pl_num(c: &[u8], p: usize) -> R<(u64, usize)> {
    let lead = *c.get(p).ok_or(Err::Eof)?;
    let n = (lead >> 6) as usize;
    if n == 0 { return Ok(((lead & 0x3F) as u64, 1)); }
    let mut v = (lead & 0x0F) as u64;
    for i in 0..n { v |= (*c.get(p + 1 + i).ok_or(Err::Eof)? as u64) << (4 + 8 * i); }
    Ok((v, 1 + n))
}

fn is_lead(b: u8) -> bool { b == b'_' || b.is_ascii_uppercase() }
fn is_name_start(b: u8) -> bool { b == b'\\' || b == b'^' || b == 0x2E || b == 0x2F || is_lead(b) }

fn name_path(c: &[u8], p: &mut usize) -> R<NamePath> {
    let mut np = NamePath { root: false, up: 0, segs: Vec::new() };
    match c.get(*p) {
        Some(b'\\') => { np.root = true; *p += 1; }
        Some(b'^') => while c.get(*p) == Some(&b'^') { np.up += 1; *p += 1; },
        _ => {}
    }
    let count = match *c.get(*p).ok_or(Err::Eof)? {
        0x00 => { *p += 1; 0 }
        0x2E => { *p += 1; 2 }
        0x2F => { *p += 1; let n = *c.get(*p).ok_or(Err::Eof)? as usize; *p += 1; n }
        b if is_lead(b) => 1,
        b => return Err(Err::BadOpcode(b, 0, *p)),
    };
    for _ in 0..count {
        if *p + 4 > c.len() { return Err(Err::Eof); }
        let mut s = [0u8; 4];
        s.copy_from_slice(&c[*p..*p + 4]);
        np.segs.push(s);
        *p += 4;
    }
    Ok(np)
}

fn seg(s: &str) -> [u8; 4] {
    let mut o = [b'_'; 4];
    for (i, c) in s.bytes().take(4).enumerate() { o[i] = c; }
    o
}

impl Default for Ns { fn default() -> Self { Self::new() } }

impl Ns {
    pub fn new() -> Ns {
        let mut ns = Ns { nodes: Vec::new(), kids: BTreeMap::new(), errors: 0, notifies: 0, depth: 0, int32: false };
        ns.nodes.push(Node { seg: *b"\\___", parent: 0, obj: Obj::Scope });
        for s in ["_GPE", "_PR_", "_SB_", "_SI_", "_TZ_"] { ns.add(0, seg(s), Obj::Scope); }
        ns.add(0, seg("_OSI"), Obj::Osi);
        ns.add(0, seg("_OS_"), Obj::Name(V::Str(b"Microsoft Windows NT".to_vec())));
        ns.add(0, seg("_REV"), Obj::Name(V::Int(2)));
        ns.add(0, seg("_GL_"), Obj::Mutex);
        ns
    }

    fn add(&mut self, parent: usize, s: [u8; 4], obj: Obj) -> usize {
        if let Some(&i) = self.kids.get(&(parent, s)) {
            // Redefinition: an External placeholder or a bare Scope takes the
            // real object; data objects are replaced (names re-created by a
            // method on every call); containers keep their node.
            let replace = matches!(self.nodes[i].obj, Obj::External { .. } | Obj::Scope)
                || matches!(obj, Obj::Name(_) | Obj::Method { .. } | Obj::Field(_) | Obj::BufField { .. } | Obj::Region { .. });
            if replace && !matches!(obj, Obj::Scope) { self.nodes[i].obj = obj; }
            return i;
        }
        let i = self.nodes.len();
        self.nodes.push(Node { seg: s, parent, obj });
        self.kids.insert((parent, s), i);
        i
    }

    pub fn len(&self) -> usize { self.nodes.len() }
    pub fn is_empty(&self) -> bool { self.nodes.len() <= 1 }
    pub fn parent(&self, n: usize) -> usize { self.nodes[n].parent }
    pub fn child(&self, n: usize, s: &str) -> Option<usize> { self.kids.get(&(n, seg(s))).copied() }
    pub fn is_device(&self, n: usize) -> bool { matches!(self.nodes[n].obj, Obj::Device | Obj::Processor | Obj::Thermal | Obj::PowerRes | Obj::Scope) }
    pub fn is_method(&self, n: usize) -> bool { matches!(self.nodes[n].obj, Obj::Method { .. }) }
    pub fn is_real_device(&self, n: usize) -> bool { matches!(self.nodes[n].obj, Obj::Device) }
    /// Region space of node `n` if it is an operation region.
    pub fn region_space(&self, n: usize) -> Option<u8> {
        if let Obj::Region { space, .. } = self.nodes[n].obj { Some(space) } else { None }
    }
    /// Children of `n`.
    pub fn children(&self, n: usize) -> Vec<usize> {
        self.kids.range((n, [0u8; 4])..=(n, [0xFFu8; 4])).map(|(_, &v)| v).collect()
    }
    pub fn seg_of(&self, n: usize) -> [u8; 4] { self.nodes[n].seg }

    /// Absolute path, e.g. `\_SB_.PCI0.LPCB.EC0_`.
    pub fn path(&self, mut n: usize) -> String {
        let mut segs = Vec::new();
        while n != 0 { segs.push(self.nodes[n].seg); n = self.nodes[n].parent; }
        let mut s = String::from("\\");
        for (i, sg) in segs.iter().rev().enumerate() {
            if i > 0 { s.push('.'); }
            for &c in sg { s.push(c as char); }
        }
        s
    }

    /// Look up an absolute dotted path (`\_SB.PCI0.LPCB`).
    pub fn find(&self, path: &str) -> Option<usize> {
        let mut n = 0usize;
        for part in path.trim_start_matches('\\').split('.').filter(|s| !s.is_empty()) {
            n = self.follow(*self.kids.get(&(n, seg(part)))?);
        }
        Some(n)
    }

    fn lookup(&self, scope: usize, np: &NamePath) -> Option<usize> {
        if np.segs.is_empty() { return if np.root { Some(0) } else { None }; }
        let mut base = if np.root { 0 } else { scope };
        for _ in 0..np.up { base = self.nodes[base].parent; }
        if !np.root && np.up == 0 && np.segs.len() == 1 {
            // Single-segment relative names search toward the root.
            let mut s = base;
            loop {
                if let Some(&i) = self.kids.get(&(s, np.segs[0])) { return Some(self.follow(i)); }
                if s == 0 { return None; }
                s = self.nodes[s].parent;
            }
        }
        let mut n = base;
        for sg in &np.segs { n = self.follow(*self.kids.get(&(n, *sg))?); }
        Some(n)
    }

    fn follow(&self, mut n: usize) -> usize {
        for _ in 0..8 { if let Obj::Alias(t) = self.nodes[n].obj { n = t; } else { break; } }
        n
    }

    /// Create (or find) the node a definition names.
    fn define(&mut self, scope: usize, np: &NamePath, obj: Obj) -> Option<usize> {
        let (last, prefix) = np.segs.split_last()?;
        let mut base = if np.root { 0 } else { scope };
        for _ in 0..np.up { base = self.nodes[base].parent; }
        for sg in prefix {
            base = match self.kids.get(&(base, *sg)) {
                Some(&i) => self.follow(i),
                None => self.add(base, *sg, Obj::Scope),
            };
        }
        Some(self.add(base, *last, obj))
    }

    fn method_argc(&self, n: usize) -> Option<u8> {
        match self.nodes[n].obj {
            Obj::Method { argc, .. } => Some(argc),
            Obj::External { argc } if argc > 0 => Some(argc),
            Obj::Osi => Some(1),
            _ => None,
        }
    }

    // ── Entry points ────────────────────────────────────────────────────

    /// Load one definition block (a whole DSDT/SSDT including its header).
    pub fn load_table(&mut self, h: &mut dyn Host, table: &'static [u8]) {
        if table.len() < 36 { return; }
        self.int32 = table[8] < 2;
        let mut f = Frame::new(0);
        let _ = self.term_list(h, &mut f, table, 36, table.len(), true);
    }

    /// Evaluate node `n`: invoke a method with `args`, or read a data object.
    pub fn eval(&mut self, h: &mut dyn Host, n: usize, args: &[V]) -> R<V> {
        let n = self.follow(n);
        match self.nodes[n].obj.clone() {
            Obj::Method { code, int32, .. } => self.invoke(h, n, code, int32, args),
            _ => self.read_node(h, n),
        }
    }

    /// Evaluate `child` of `n` (e.g. `_BST` under the battery device). None
    /// when the child is missing or evaluation failed (logged).
    pub fn eval_child(&mut self, h: &mut dyn Host, n: usize, child: &str, args: &[V]) -> Option<V> {
        let c = self.child(n, child)?;
        match self.eval(h, c, args) {
            Ok(v) => Some(v),
            Err(e) => {
                self.errors += 1;
                if self.errors < 64 {
                    let msg = alloc::format!("acpi: {}.{} failed: {:?}\n", self.path(n), child, e);
                    h.log(&msg);
                }
                None
            }
        }
    }

    /// Resolve a package element or reference to a plain value.
    pub fn resolve(&mut self, h: &mut dyn Host, v: &V) -> V {
        match v {
            V::Node(n) if !self.is_device(*n) => self.eval(h, *n, &[]).unwrap_or(V::Uninit),
            o => self.rv(o.clone()),
        }
    }

    /// An integer from a value (package element), resolving references.
    pub fn int_of(&mut self, h: &mut dyn Host, v: &V) -> Option<u64> {
        let v = self.resolve(h, v);
        self.to_int(&v).ok()
    }

    fn invoke(&mut self, h: &mut dyn Host, n: usize, code: &'static [u8], int32: bool, args: &[V]) -> R<V> {
        if self.depth >= 48 { return Err(Err::Depth); }
        self.depth += 1;
        let saved = self.int32;
        self.int32 = int32;
        let mut f = Frame::new(n);
        for (i, a) in args.iter().take(7).enumerate() { f.args[i] = a.clone(); }
        let r = self.term_list(h, &mut f, code, 0, code.len(), false);
        self.int32 = saved;
        self.depth -= 1;
        match r? { Flow::Ret(v) => Ok(v), _ => Ok(V::Uninit) }
    }

    // ── Interpreter core ────────────────────────────────────────────────

    fn mask(&self, v: u64) -> u64 { if self.int32 { v & 0xFFFF_FFFF } else { v } }
    fn ones(&self) -> u64 { self.mask(u64::MAX) }

    fn term_list(&mut self, h: &mut dyn Host, f: &mut Frame, c: &'static [u8], mut p: usize, end: usize, load: bool) -> R<Flow> {
        while p < end {
            let start = p;
            match self.statement(h, f, c, &mut p, load) {
                Ok(Flow::Next) => {}
                Ok(fl) => return Ok(fl),
                Err(e) => {
                    if load {
                        // Nothing after an unparsable term in this list can be
                        // parsed reliably: give up on the rest of the list.
                        self.errors += 1;
                        if self.errors <= 12 {
                            let msg = alloc::format!("acpi: load error {:?} at +{:#x} in {}\n", e, start, self.path(f.scope));
                            h.log(&msg);
                        }
                        return Ok(Flow::Next);
                    }
                    return Err(e);
                }
            }
        }
        Ok(Flow::Next)
    }

    fn body(&mut self, h: &mut dyn Host, f: &mut Frame, n: usize, c: &'static [u8], p: usize, end: usize, load: bool) -> R<Flow> {
        let saved = f.scope;
        f.scope = n;
        let r = self.term_list(h, f, c, p, end, load);
        f.scope = saved;
        r
    }

    /// One TermObj: a statement, a definition, or an expression whose value
    /// is discarded.
    fn statement(&mut self, h: &mut dyn Host, f: &mut Frame, c: &'static [u8], p: &mut usize, load: bool) -> R<Flow> {
        let op = *c.get(*p).ok_or(Err::Eof)?;
        match op {
            0x10 => { // Scope
                *p += 1;
                let end = pkg_len(c, p)?;
                let np = name_path(c, p)?;
                let n = match self.lookup(f.scope, &np) { Some(n) => n, None => self.define(f.scope, &np, Obj::Scope).ok_or(Err::Type)? };
                let fl = self.body(h, f, n, c, *p, end, load)?;
                *p = end;
                if !load { if let Flow::Ret(_) = fl { return Ok(fl); } }
            }
            0x14 => { // Method
                *p += 1;
                let end = pkg_len(c, p)?;
                let np = name_path(c, p)?;
                let flags = *c.get(*p).ok_or(Err::Eof)?;
                *p += 1;
                let code = &c[*p..end];
                self.define(f.scope, &np, Obj::Method { code, argc: flags & 7, int32: self.int32 });
                *p = end;
            }
            0x08 => { // Name
                *p += 1;
                let np = name_path(c, p)?;
                let v = self.term(h, f, c, p)?;
                self.define(f.scope, &np, Obj::Name(v.deep()));
            }
            0x06 => { // Alias
                *p += 1;
                let src = name_path(c, p)?;
                let dst = name_path(c, p)?;
                if let Some(t) = self.lookup(f.scope, &src) { self.define(f.scope, &dst, Obj::Alias(t)); }
            }
            0x15 => { // External
                *p += 1;
                let np = name_path(c, p)?;
                let kind = *c.get(*p).ok_or(Err::Eof)?;
                let argc = *c.get(*p + 1).ok_or(Err::Eof)?;
                *p += 2;
                if kind == 8 && self.lookup(f.scope, &np).is_none() {
                    self.define(f.scope, &np, Obj::External { argc });
                }
            }
            0xA0 => { // If / Else
                *p += 1;
                let end = pkg_len(c, p)?;
                let t = self.int_op(h, f, c, p)? != 0;
                let mut flow = Flow::Next;
                if t { flow = self.term_list(h, f, c, *p, end, load)?; }
                *p = end;
                if c.get(*p) == Some(&0xA1) {
                    *p += 1;
                    let eend = pkg_len(c, p)?;
                    if !t { flow = self.term_list(h, f, c, *p, eend, load)?; }
                    *p = eend;
                }
                return Ok(flow);
            }
            0xA1 => { *p += 1; let end = pkg_len(c, p)?; *p = end; } // stray Else
            0xA2 => { // While
                *p += 1;
                let end = pkg_len(c, p)?;
                let cs = *p;
                let mut iters = 0u32;
                loop {
                    let mut q = cs;
                    if self.int_op(h, f, c, &mut q)? == 0 { break; }
                    iters += 1;
                    if iters > 100_000 { return Err(Err::Loop); }
                    match self.term_list(h, f, c, q, end, load)? {
                        Flow::Break => break,
                        Flow::Ret(v) => { *p = end; return Ok(Flow::Ret(v)); }
                        _ => {}
                    }
                }
                *p = end;
            }
            0xA4 => { // Return
                *p += 1;
                let v = self.term(h, f, c, p)?;
                let v = self.rv(v);
                return Ok(Flow::Ret(v));
            }
            0xA5 => { *p += 1; return Ok(Flow::Break); }
            0x9F => { *p += 1; return Ok(Flow::Cont); }
            0xA3 | 0xCC => { *p += 1; }
            0x86 => { // Notify
                *p += 1;
                let t = self.super_name(h, f, c, p)?;
                let code = self.int_op(h, f, c, p)?;
                let n = match t { Tgt::Node(n) => Some(n), Tgt::Ref(V::Node(n)) => Some(n), Tgt::Arg(i) | Tgt::Local(i) if false => Some(i), _ => None };
                if let Some(n) = n { self.notifies += 1; h.notify(n, code); }
            }
            0x5B => {
                let op2 = *c.get(*p + 1).ok_or(Err::Eof)?;
                match op2 {
                    0x82 | 0x85 | 0x83 | 0x84 => { // Device, ThermalZone, Processor, PowerResource
                        *p += 2;
                        let end = pkg_len(c, p)?;
                        let np = name_path(c, p)?;
                        let obj = match op2 { 0x82 => Obj::Device, 0x85 => Obj::Thermal, 0x83 => { *p += 6; Obj::Processor } _ => { *p += 3; Obj::PowerRes } };
                        let n = self.define(f.scope, &np, obj).ok_or(Err::Type)?;
                        self.body(h, f, n, c, *p, end, load)?;
                        *p = end;
                    }
                    0x01 => { *p += 2; let np = name_path(c, p)?; *p += 1; self.define(f.scope, &np, Obj::Mutex); }
                    0x02 => { *p += 2; let np = name_path(c, p)?; self.define(f.scope, &np, Obj::Event); }
                    0x80 => { // OperationRegion
                        *p += 2;
                        let np = name_path(c, p)?;
                        let space = *c.get(*p).ok_or(Err::Eof)?;
                        *p += 1;
                        let base = self.int_op(h, f, c, p)?;
                        let _len = self.int_op(h, f, c, p)?;
                        self.define(f.scope, &np, Obj::Region { space, base });
                    }
                    0x88 => { // DataTableRegion: unsupported, reads as 0
                        *p += 2;
                        let np = name_path(c, p)?;
                        for _ in 0..3 { self.term(h, f, c, p)?; }
                        self.define(f.scope, &np, Obj::Region { space: 0xFF, base: 0 });
                    }
                    0x81 | 0x86 | 0x87 => { *p += 2; self.field(h, f, c, p, op2)?; }
                    0x13 => { // CreateField
                        *p += 2;
                        let src = self.term(h, f, c, p)?;
                        let bi = self.int_op(h, f, c, p)? as usize;
                        let nb = self.int_op(h, f, c, p)? as usize;
                        let np = name_path(c, p)?;
                        let buf = self.buf_of(src)?;
                        self.define(f.scope, &np, Obj::BufField { buf, bit_off: bi, bits: nb });
                    }
                    0x21 => { *p += 2; let us = self.int_op(h, f, c, p)?; h.stall_us(us.min(1000)); }
                    0x22 => { *p += 2; let ms = self.int_op(h, f, c, p)?; h.sleep_ms(ms.min(2000)); }
                    0x24 | 0x26 | 0x27 => { *p += 2; self.super_name(h, f, c, p)?; } // Signal, Reset, Release
                    0x32 => { *p += 7; self.term(h, f, c, p)?; return Err(Err::Fatal); }
                    0x20 => { *p += 2; name_path(c, p)?; self.target(h, f, c, p)?; } // Load: ignored
                    0x30 => { *p += 2; } // Unload-ish/Revision as a statement: ignore
                    _ => { self.term(h, f, c, p)?; }
                }
            }
            _ => { self.term(h, f, c, p)?; }
        }
        Ok(Flow::Next)
    }

    fn field(&mut self, h: &mut dyn Host, f: &mut Frame, c: &'static [u8], p: &mut usize, op2: u8) -> R<()> {
        let end = pkg_len(c, p)?;
        let a = name_path(c, p)?;
        let kind = match op2 {
            0x81 => FieldKind::Region(self.lookup(f.scope, &a).ok_or(Err::Undefined(*p))?),
            0x86 => {
                let d = name_path(c, p)?;
                FieldKind::Index {
                    index: self.lookup(f.scope, &a).ok_or(Err::Undefined(*p))?,
                    data: self.lookup(f.scope, &d).ok_or(Err::Undefined(*p))?,
                }
            }
            _ => {
                let b = name_path(c, p)?;
                let v = self.int_op(h, f, c, p)?;
                FieldKind::Bank {
                    region: self.lookup(f.scope, &a).ok_or(Err::Undefined(*p))?,
                    bank: self.lookup(f.scope, &b).ok_or(Err::Undefined(*p))?,
                    val: v,
                }
            }
        };
        let flags = *c.get(*p).ok_or(Err::Eof)?;
        *p += 1;
        let mut access = flags & 0x0F;
        let update = (flags >> 5) & 3;
        let mut bit = 0u64;
        while *p < end {
            match c[*p] {
                0x00 => { let (v, n) = pl_num(c, *p + 1)?; bit += v; *p += 1 + n; }
                0x01 => { access = c.get(*p + 1).copied().unwrap_or(0) & 0x0F; *p += 3; }
                0x02 => { *p += 1; if c.get(*p).map_or(false, |&b| is_name_start(b)) { name_path(c, p)?; } else { self.term(h, f, c, p)?; } }
                0x03 => { access = c.get(*p + 1).copied().unwrap_or(0) & 0x0F; *p += 4; }
                _ => {
                    if *p + 4 > end { return Err(Err::Eof); }
                    let mut s = [0u8; 4];
                    s.copy_from_slice(&c[*p..*p + 4]);
                    let (bits, n) = pl_num(c, *p + 4)?;
                    *p += 4 + n;
                    let fu = FieldUnit { kind, bit_off: bit, bits, access, update };
                    let np = NamePath { root: false, up: 0, segs: vec![s] };
                    self.define(f.scope, &np, Obj::Field(fu));
                    bit += bits;
                }
            }
        }
        *p = end;
        Ok(())
    }

    fn buf_of(&mut self, v: V) -> R<Rc<RefCell<Vec<u8>>>> {
        match self.rv(v) {
            V::Buf(b) => Ok(b),
            V::Str(s) => Ok(Rc::new(RefCell::new(s))),
            V::Int(i) => Ok(Rc::new(RefCell::new(i.to_le_bytes().to_vec()))),
            _ => Err(Err::Type),
        }
    }

    // ── Values ──────────────────────────────────────────────────────────

    fn to_int(&self, v: &V) -> R<u64> {
        Ok(match v {
            V::Int(i) => *i,
            V::Uninit => 0,
            V::Str(s) => {
                let mut x = 0u64;
                let s: &[u8] = if s.len() > 2 && s[0] == b'0' && (s[1] | 0x20) == b'x' { &s[2..] } else { s };
                for &ch in s {
                    let d = match ch { b'0'..=b'9' => ch - b'0', b'a'..=b'f' => ch - b'a' + 10, b'A'..=b'F' => ch - b'A' + 10, _ => break };
                    x = (x << 4) | d as u64;
                }
                self.mask(x)
            }
            V::Buf(b) => {
                let b = b.borrow();
                let n = if self.int32 { 4 } else { 8 };
                let mut x = 0u64;
                for (i, &byte) in b.iter().take(n).enumerate() { x |= (byte as u64) << (8 * i); }
                x
            }
            V::BufIdx(b, i) => b.borrow().get(*i).copied().unwrap_or(0) as u64,
            V::PkgIdx(pk, i) => { let e = pk.borrow().get(*i).cloned().unwrap_or(V::Uninit); return self.to_int(&e); }
            _ => return Err(Err::Type),
        })
    }

    fn to_bytes(&self, v: &V) -> R<Vec<u8>> {
        Ok(match v {
            V::Int(i) => i.to_le_bytes()[..if self.int32 { 4 } else { 8 }].to_vec(),
            V::Str(s) => s.clone(),
            V::Buf(b) => b.borrow().clone(),
            V::Uninit => Vec::new(),
            o => { let x = self.to_int(o)?; x.to_le_bytes()[..if self.int32 { 4 } else { 8 }].to_vec() }
        })
    }

    /// Dereference Index results to the element value.
    fn rv(&self, v: V) -> V {
        match v {
            V::BufIdx(b, i) => V::Int(b.borrow().get(i).copied().unwrap_or(0) as u64),
            V::PkgIdx(pk, i) => pk.borrow().get(i).cloned().unwrap_or(V::Uninit),
            o => o,
        }
    }

    fn read_node(&mut self, h: &mut dyn Host, n: usize) -> R<V> {
        let n = self.follow(n);
        match self.nodes[n].obj.clone() {
            Obj::Name(v) => Ok(v),
            Obj::Field(fu) => self.read_field(h, &fu),
            Obj::BufField { buf, bit_off, bits } => {
                let b = buf.borrow();
                Ok(bytes_value(extract_bits(&b, bit_off, bits), bits))
            }
            _ => Ok(V::Node(n)),
        }
    }

    fn read_field(&mut self, h: &mut dyn Host, fu: &FieldUnit) -> R<V> {
        let w = self.access_width(fu);
        let mut out = vec![0u8; fu.bits.div_ceil(8) as usize];
        let mut unit = fu.bit_off / w * w;
        while unit < fu.bit_off + fu.bits {
            let raw = self.unit_read(h, fu, unit / 8, w as u8)?;
            let lo = fu.bit_off.max(unit);
            let hi = (fu.bit_off + fu.bits).min(unit + w);
            for b in lo..hi {
                if (raw >> (b - unit)) & 1 != 0 {
                    let ob = b - fu.bit_off;
                    out[(ob / 8) as usize] |= 1 << (ob % 8);
                }
            }
            unit += w;
        }
        Ok(bytes_value(out, fu.bits as usize))
    }

    fn write_field(&mut self, h: &mut dyn Host, fu: &FieldUnit, v: &V) -> R<()> {
        let src = self.to_bytes(v)?;
        let w = self.access_width(fu);
        let mut unit = fu.bit_off / w * w;
        while unit < fu.bit_off + fu.bits {
            let lo = fu.bit_off.max(unit);
            let hi = (fu.bit_off + fu.bits).min(unit + w);
            let full = lo == unit && hi == unit + w;
            let mut raw = if full { 0 } else {
                match fu.update { 1 => u64::MAX, 2 => 0, _ => self.unit_read(h, fu, unit / 8, w as u8)? }
            };
            for b in lo..hi {
                let sb = b - fu.bit_off;
                let bit = src.get((sb / 8) as usize).map_or(0, |&x| (x >> (sb % 8)) & 1);
                let m = 1u64 << (b - unit);
                if bit != 0 { raw |= m } else { raw &= !m }
            }
            if w < 64 { raw &= (1u64 << w) - 1; }
            self.unit_write(h, fu, unit / 8, w as u8, raw)?;
            unit += w;
        }
        Ok(())
    }

    fn access_width(&self, fu: &FieldUnit) -> u64 {
        let space = match fu.kind {
            FieldKind::Region(r) | FieldKind::Bank { region: r, .. } => self.region_space(r).unwrap_or(0),
            FieldKind::Index { .. } => 0xFE,
        };
        match fu.access {
            1 => 8, 2 => 16, 3 => 32, 4 => 64,
            _ if space == SPACE_EC => 8,
            _ => {
                if fu.bit_off % 32 == 0 && fu.bits % 32 == 0 { 32 }
                else if fu.bit_off % 16 == 0 && fu.bits % 16 == 0 { 16 }
                else { 8 }
            }
        }
    }

    fn region_addr(&mut self, h: &mut dyn Host, r: usize, off: u64) -> R<(u8, u64)> {
        let (space, base) = match self.nodes[r].obj { Obj::Region { space, base } => (space, base), _ => return Err(Err::Type) };
        if space == SPACE_PCI {
            // Device from the parent's _ADR; bus from the nearest _BBN.
            let dev = self.nodes[r].parent;
            let adr = self.eval_child(h, dev, "_ADR", &[]).and_then(|v| v.as_int()).unwrap_or(0);
            let mut bus = 0u64;
            let mut d = dev;
            while d != 0 {
                if self.child(d, "_BBN").is_some() { bus = self.eval_child(h, d, "_BBN", &[]).and_then(|v| v.as_int()).unwrap_or(0); break; }
                d = self.nodes[d].parent;
            }
            return Ok((space, (bus << 32) | (((adr >> 16) & 0x1F) << 24) | ((adr & 7) << 16) | ((base + off) & 0xFFF)));
        }
        Ok((space, base.wrapping_add(off)))
    }

    fn unit_read(&mut self, h: &mut dyn Host, fu: &FieldUnit, byte: u64, w: u8) -> R<u64> {
        match fu.kind {
            FieldKind::Region(r) => {
                let (s, a) = self.region_addr(h, r, byte)?;
                Ok(if s <= SPACE_EC { h.read(s, a, w) } else { 0 })
            }
            FieldKind::Bank { region, bank, val } => {
                self.store_node(h, bank, &V::Int(val))?;
                let (s, a) = self.region_addr(h, region, byte)?;
                Ok(if s <= SPACE_EC { h.read(s, a, w) } else { 0 })
            }
            FieldKind::Index { index, data } => {
                self.store_node(h, index, &V::Int(byte))?;
                let v = self.read_node(h, data)?;
                self.to_int(&v)
            }
        }
    }

    fn unit_write(&mut self, h: &mut dyn Host, fu: &FieldUnit, byte: u64, w: u8, val: u64) -> R<()> {
        match fu.kind {
            FieldKind::Region(r) => {
                let (s, a) = self.region_addr(h, r, byte)?;
                if s <= SPACE_EC { h.write(s, a, w, val); }
            }
            FieldKind::Bank { region, bank, val: bv } => {
                self.store_node(h, bank, &V::Int(bv))?;
                let (s, a) = self.region_addr(h, region, byte)?;
                if s <= SPACE_EC { h.write(s, a, w, val); }
            }
            FieldKind::Index { index, data } => {
                self.store_node(h, index, &V::Int(byte))?;
                self.store_node(h, data, &V::Int(val))?;
            }
        }
        Ok(())
    }

    fn store_node(&mut self, h: &mut dyn Host, n: usize, v: &V) -> R<()> {
        let n = self.follow(n);
        match self.nodes[n].obj.clone() {
            Obj::Field(fu) => self.write_field(h, &fu, v),
            Obj::BufField { buf, bit_off, bits } => {
                let src = self.to_bytes(v)?;
                let mut b = buf.borrow_mut();
                for i in 0..bits {
                    let bit = src.get(i / 8).map_or(0, |&x| (x >> (i % 8)) & 1);
                    let t = bit_off + i;
                    if t / 8 >= b.len() { break; }
                    if bit != 0 { b[t / 8] |= 1 << (t % 8) } else { b[t / 8] &= !(1 << (t % 8)) }
                }
                Ok(())
            }
            Obj::Name(old) => {
                let nv = match (&old, v) {
                    (V::Int(_), V::Buf(_) | V::Str(_)) => V::Int(self.to_int(v)?),
                    (V::Buf(ob), V::Int(_)) => {
                        let mut bytes = self.to_bytes(v)?;
                        bytes.resize(ob.borrow().len(), 0);
                        V::buf(bytes)
                    }
                    _ => v.deep(),
                };
                self.nodes[n].obj = Obj::Name(nv);
                Ok(())
            }
            Obj::External { .. } => { self.nodes[n].obj = Obj::Name(v.deep()); Ok(()) }
            _ => Ok(()),
        }
    }

    fn store(&mut self, h: &mut dyn Host, f: &mut Frame, t: &Tgt, v: &V) -> R<()> {
        let v = self.rv(v.clone());
        match t {
            Tgt::None => Ok(()),
            Tgt::Debug => { let s = alloc::format!("acpi: debug {:?}\n", v); h.log(&s); Ok(()) }
            Tgt::Local(i) => { f.locals[*i] = v.deep(); Ok(()) }
            Tgt::Arg(i) => match f.args[*i].clone() {
                r @ (V::BufIdx(..) | V::PkgIdx(..)) => self.store(h, f, &Tgt::Ref(r), &v),
                V::Node(n) if !self.is_device(n) && !self.is_method(n) => self.store_node(h, n, &v),
                _ => { f.args[*i] = v.deep(); Ok(()) }
            },
            Tgt::Node(n) => self.store_node(h, *n, &v),
            Tgt::Ref(r) => match r {
                V::BufIdx(b, i) => {
                    let x = self.to_int(&v)? as u8;
                    if let Some(e) = b.borrow_mut().get_mut(*i) { *e = x; }
                    Ok(())
                }
                V::PkgIdx(pk, i) => {
                    if let Some(e) = pk.borrow_mut().get_mut(*i) { *e = v.deep(); }
                    Ok(())
                }
                V::Node(n) => self.store_node(h, *n, &v),
                _ => Ok(()),
            },
        }
    }

    fn load_tgt(&mut self, h: &mut dyn Host, f: &mut Frame, t: &Tgt) -> R<V> {
        Ok(match t {
            Tgt::Local(i) => self.rv(f.locals[*i].clone()),
            Tgt::Arg(i) => match f.args[*i].clone() {
                V::Node(n) if !self.is_device(n) && !self.is_method(n) => self.read_node(h, n)?,
                o => self.rv(o),
            },
            Tgt::Node(n) => self.read_node(h, *n)?,
            Tgt::Ref(r) => match r { V::Node(n) => self.read_node(h, *n)?, o => self.rv(o.clone()) },
            _ => V::Uninit,
        })
    }

    /// Target operand (NullName allowed).
    fn target(&mut self, h: &mut dyn Host, f: &mut Frame, c: &'static [u8], p: &mut usize) -> R<Tgt> {
        if c.get(*p) == Some(&0x00) { *p += 1; return Ok(Tgt::None); }
        self.super_name(h, f, c, p)
    }

    fn super_name(&mut self, h: &mut dyn Host, f: &mut Frame, c: &'static [u8], p: &mut usize) -> R<Tgt> {
        let op = *c.get(*p).ok_or(Err::Eof)?;
        match op {
            0x60..=0x67 => { *p += 1; Ok(Tgt::Local((op - 0x60) as usize)) }
            0x68..=0x6E => { *p += 1; Ok(Tgt::Arg((op - 0x68) as usize)) }
            0x00 => { *p += 1; Ok(Tgt::None) }
            0x5B if c.get(*p + 1) == Some(&0x31) => { *p += 2; Ok(Tgt::Debug) }
            b if is_name_start(b) => {
                let at = *p;
                let np = name_path(c, p)?;
                match self.lookup(f.scope, &np) {
                    Some(n) if self.method_argc(n).is_some() => {
                        // A method call yielding a reference.
                        *p = at;
                        Ok(Tgt::Ref(self.term(h, f, c, p)?))
                    }
                    Some(n) => Ok(Tgt::Node(n)),
                    None => Err(Err::Undefined(at)),
                }
            }
            _ => Ok(Tgt::Ref(self.term(h, f, c, p)?)), // Index / DerefOf / RefOf
        }
    }

    fn int_op(&mut self, h: &mut dyn Host, f: &mut Frame, c: &'static [u8], p: &mut usize) -> R<u64> {
        let v = self.val_op(h, f, c, p)?;
        self.to_int(&v)
    }

    /// A TermArg evaluated to a plain value (references dereferenced).
    fn val_op(&mut self, h: &mut dyn Host, f: &mut Frame, c: &'static [u8], p: &mut usize) -> R<V> {
        let v = self.term(h, f, c, p)?;
        Ok(match self.rv(v) { V::Node(n) if !self.is_device(n) && !self.is_method(n) => self.read_node(h, n)?, o => o })
    }

    /// Evaluate one TermArg and return its value.
    fn term(&mut self, h: &mut dyn Host, f: &mut Frame, c: &'static [u8], p: &mut usize) -> R<V> {
        let at = *p;
        let op = *c.get(*p).ok_or(Err::Eof)?;
        *p += 1;
        let ones = self.ones();
        Ok(match op {
            0x00 => V::Int(0),
            0x01 => V::Int(1),
            0xFF => V::Int(ones),
            0x0A => { let v = *c.get(*p).ok_or(Err::Eof)? as u64; *p += 1; V::Int(v) }
            0x0B => { let v = rd_le(c, *p, 2)?; *p += 2; V::Int(v) }
            0x0C => { let v = rd_le(c, *p, 4)?; *p += 4; V::Int(v) }
            0x0E => { let v = rd_le(c, *p, 8)?; *p += 8; V::Int(self.mask(v)) }
            0x0D => {
                let s = *p;
                while *c.get(*p).ok_or(Err::Eof)? != 0 { *p += 1; }
                let v = c[s..*p].to_vec();
                *p += 1;
                V::Str(v)
            }
            0x11 => { // Buffer
                let end = pkg_len(c, p)?;
                let n = self.int_op(h, f, c, p)? as usize;
                let mut b = c[*p..end].to_vec();
                if b.len() < n { b.resize(n, 0); }
                *p = end;
                V::buf(b)
            }
            0x12 | 0x13 => { // Package / VarPackage
                let end = pkg_len(c, p)?;
                let n = if op == 0x12 { let n = *c.get(*p).ok_or(Err::Eof)? as usize; *p += 1; n } else { self.int_op(h, f, c, p)? as usize };
                let mut el = Vec::new();
                while *p < end {
                    if is_name_start(c[*p]) {
                        let np = name_path(c, p)?;
                        el.push(match self.lookup(f.scope, &np) { Some(x) => V::Node(x), None => V::Str(np_str(&np)) });
                    } else {
                        el.push(self.term(h, f, c, p)?);
                    }
                }
                *p = end;
                while el.len() < n { el.push(V::Uninit); }
                V::pkg(el)
            }
            0x60..=0x67 => f.locals[(op - 0x60) as usize].clone(),
            0x68..=0x6E => match f.args[(op - 0x68) as usize].clone() {
                V::Node(n) if !self.is_device(n) && !self.is_method(n) => self.read_node(h, n)?,
                o => o,
            },
            0x70 => { // Store
                let v = self.val_op(h, f, c, p)?;
                let t = self.super_name(h, f, c, p)?;
                self.store(h, f, &t, &v)?;
                v
            }
            0x9D => { // CopyObject
                let v = self.val_op(h, f, c, p)?;
                let t = self.super_name(h, f, c, p)?;
                match t {
                    Tgt::Node(n) => { self.nodes[n].obj = Obj::Name(v.deep()); }
                    t => self.store(h, f, &t, &v)?,
                }
                v
            }
            0x71 => { // RefOf
                match self.super_name(h, f, c, p)? {
                    Tgt::Node(n) => V::Node(n),
                    Tgt::Ref(r) => r,
                    Tgt::Arg(i) => f.args[i].clone(),
                    Tgt::Local(i) => f.locals[i].clone(),
                    _ => V::Uninit,
                }
            }
            0x83 => { // DerefOf
                let v = self.term(h, f, c, p)?;
                match v {
                    V::Node(n) => self.read_node(h, n)?,
                    V::Str(s) => {
                        let mut q = 0usize;
                        let np = name_path(&s, &mut q)?;
                        let n = self.lookup(f.scope, &np).ok_or(Err::Undefined(at))?;
                        self.read_node(h, n)?
                    }
                    o => match self.rv(o) { V::Node(n) if !self.is_device(n) => self.read_node(h, n)?, x => x },
                }
            }
            0x72 | 0x74 | 0x77 | 0x79 | 0x7A | 0x7B | 0x7C | 0x7D | 0x7E | 0x7F | 0x85 => {
                let a = self.int_op(h, f, c, p)?;
                let b = self.int_op(h, f, c, p)?;
                let t = self.target(h, f, c, p)?;
                let r = match op {
                    0x72 => a.wrapping_add(b),
                    0x74 => a.wrapping_sub(b),
                    0x77 => a.wrapping_mul(b),
                    0x79 => if b >= 64 { 0 } else { a << b },
                    0x7A => if b >= 64 { 0 } else { a >> b },
                    0x7B => a & b,
                    0x7C => !(a & b),
                    0x7D => a | b,
                    0x7E => !(a | b),
                    0x7F => a ^ b,
                    _ => if b == 0 { return Err(Err::Type) } else { a % b },
                };
                let r = V::Int(self.mask(r));
                self.store(h, f, &t, &r)?;
                r
            }
            0x78 => { // Divide
                let a = self.int_op(h, f, c, p)?;
                let b = self.int_op(h, f, c, p)?;
                let rt = self.target(h, f, c, p)?;
                let qt = self.target(h, f, c, p)?;
                if b == 0 { return Err(Err::Type); }
                self.store(h, f, &rt, &V::Int(a % b))?;
                let q = V::Int(a / b);
                self.store(h, f, &qt, &q)?;
                q
            }
            0x80..=0x82 => { // Not, FindSetLeftBit, FindSetRightBit
                let a = self.int_op(h, f, c, p)?;
                let t = self.target(h, f, c, p)?;
                let r = match op {
                    0x80 => self.mask(!a),
                    0x81 => if a == 0 { 0 } else { 64 - a.leading_zeros() as u64 },
                    _ => if a == 0 { 0 } else { a.trailing_zeros() as u64 + 1 },
                };
                self.store(h, f, &t, &V::Int(r))?;
                V::Int(r)
            }
            0x75 | 0x76 => { // Increment / Decrement
                let t = self.super_name(h, f, c, p)?;
                let v = self.load_tgt(h, f, &t)?;
                let v = self.to_int(&v)?;
                let r = V::Int(self.mask(if op == 0x75 { v.wrapping_add(1) } else { v.wrapping_sub(1) }));
                self.store(h, f, &t, &r)?;
                r
            }
            0x90 | 0x91 => {
                let a = self.int_op(h, f, c, p)?;
                let b = self.int_op(h, f, c, p)?;
                V::Int(if (op == 0x90 && a != 0 && b != 0) || (op == 0x91 && (a != 0 || b != 0)) { ones } else { 0 })
            }
            0x92 => {
                let nx = c.get(*p).copied().unwrap_or(0);
                if (0x93..=0x95).contains(&nx) {
                    *p += 1;
                    let o = self.cmp(h, f, c, p)?;
                    let r = match nx { 0x93 => o != Ord::Equal, 0x94 => o != Ord::Greater, _ => o != Ord::Less };
                    V::Int(if r { ones } else { 0 })
                } else {
                    let a = self.int_op(h, f, c, p)?;
                    V::Int(if a == 0 { ones } else { 0 })
                }
            }
            0x93..=0x95 => {
                let o = self.cmp(h, f, c, p)?;
                let r = match op { 0x93 => o == Ord::Equal, 0x94 => o == Ord::Greater, _ => o == Ord::Less };
                V::Int(if r { ones } else { 0 })
            }
            0x73 | 0x84 => { // Concatenate / ConcatenateResTemplate
                let a = self.val_op(h, f, c, p)?;
                let b = self.val_op(h, f, c, p)?;
                let t = self.target(h, f, c, p)?;
                let r = match (&a, op) {
                    (V::Str(s), 0x73) => {
                        let mut s = s.clone();
                        match &b {
                            V::Int(i) => s.extend_from_slice(hex_str(*i, self.int32).as_bytes()),
                            o => s.extend(self.to_bytes(o)?.into_iter().take_while(|&x| x != 0)),
                        }
                        V::Str(s)
                    }
                    (V::Int(_), 0x73) => {
                        let mut v = self.to_bytes(&a)?;
                        let bi = self.to_int(&b).unwrap_or(0);
                        v.extend(self.to_bytes(&V::Int(bi))?);
                        V::buf(v)
                    }
                    _ => {
                        let mut v = self.to_bytes(&a)?;
                        if op == 0x84 && v.len() >= 2 && v[v.len() - 2] == 0x79 { v.truncate(v.len() - 2); }
                        v.extend(self.to_bytes(&b)?);
                        V::buf(v)
                    }
                };
                self.store(h, f, &t, &r)?;
                r
            }
            0x87 => { // SizeOf
                let t = self.super_name(h, f, c, p)?;
                let v = self.load_tgt(h, f, &t)?;
                V::Int(match v {
                    V::Str(s) => s.len() as u64,
                    V::Buf(b) => b.borrow().len() as u64,
                    V::Pkg(pk) => pk.borrow().len() as u64,
                    _ => return Err(Err::Type),
                })
            }
            0x88 => { // Index
                let src = self.val_op(h, f, c, p)?;
                let i = self.int_op(h, f, c, p)? as usize;
                let t = self.target(h, f, c, p)?;
                let r = match src {
                    V::Buf(b) => V::BufIdx(b, i),
                    V::Pkg(pk) => V::PkgIdx(pk, i),
                    V::Str(s) => V::BufIdx(Rc::new(RefCell::new(s)), i),
                    _ => return Err(Err::Type),
                };
                match t {
                    Tgt::None => {}
                    Tgt::Local(l) => f.locals[l] = r.clone(),
                    Tgt::Arg(a) => f.args[a] = r.clone(),
                    Tgt::Node(n) => self.nodes[n].obj = Obj::Name(r.clone()),
                    _ => {}
                }
                r
            }
            0x89 => { // Match
                let pk = self.val_op(h, f, c, p)?;
                let o1 = *c.get(*p).ok_or(Err::Eof)?; *p += 1;
                let v1 = self.int_op(h, f, c, p)?;
                let o2 = *c.get(*p).ok_or(Err::Eof)?; *p += 1;
                let v2 = self.int_op(h, f, c, p)?;
                let start = self.int_op(h, f, c, p)? as usize;
                let mut res = ones;
                if let V::Pkg(pk) = pk {
                    let els = pk.borrow().clone();
                    for (i, e) in els.iter().enumerate().skip(start) {
                        let Ok(e) = self.to_int(e) else { continue };
                        if mop(o1, e, v1) && mop(o2, e, v2) { res = i as u64; break; }
                    }
                }
                V::Int(res)
            }
            0x8A..=0x8D | 0x8F => { // Create{DWord,Word,Byte,Bit,QWord}Field
                let src = self.val_op(h, f, c, p)?;
                let idx = self.int_op(h, f, c, p)? as usize;
                let np = name_path(c, p)?;
                let (off, bits) = match op { 0x8A => (idx * 8, 32), 0x8B => (idx * 8, 16), 0x8C => (idx * 8, 8), 0x8D => (idx, 1), _ => (idx * 8, 64) };
                let buf = self.buf_of(src)?;
                self.define(f.scope, &np, Obj::BufField { buf, bit_off: off, bits });
                V::Uninit
            }
            0x8E => { // ObjectType
                let k = match self.super_name(h, f, c, p)? {
                    Tgt::Node(n) => match &self.nodes[n].obj {
                        Obj::Name(v) => vtype(v), Obj::Field(_) => 5, Obj::Device => 6, Obj::Event => 7, Obj::Method { .. } => 8,
                        Obj::Mutex => 9, Obj::Region { .. } => 10, Obj::PowerRes => 11, Obj::Processor => 12, Obj::Thermal => 13,
                        Obj::BufField { .. } => 14, _ => 0,
                    },
                    Tgt::Local(i) => vtype(&f.locals[i]),
                    Tgt::Arg(i) => vtype(&f.args[i]),
                    _ => 0,
                };
                V::Int(k)
            }
            0x96 => { // ToBuffer
                let v = self.val_op(h, f, c, p)?;
                let mut b = self.to_bytes(&v)?;
                if let V::Str(_) = v { b.push(0); }
                let r = V::buf(b);
                let t = self.target(h, f, c, p)?;
                self.store(h, f, &t, &r)?;
                r
            }
            0x97 | 0x98 => { // ToDecimalString / ToHexString
                let v = self.val_op(h, f, c, p)?;
                let s = match &v {
                    V::Int(i) => if op == 0x97 { alloc::format!("{}", i) } else { hex_str(*i, self.int32) },
                    V::Buf(b) => {
                        let parts: Vec<String> = b.borrow().iter().map(|x| if op == 0x97 { alloc::format!("{}", x) } else { alloc::format!("0x{:02X}", x) }).collect();
                        parts.join(",")
                    }
                    V::Str(s) => String::from_utf8_lossy(s).into_owned(),
                    _ => return Err(Err::Type),
                };
                let r = V::Str(s.into_bytes());
                let t = self.target(h, f, c, p)?;
                self.store(h, f, &t, &r)?;
                r
            }
            0x99 => { // ToInteger
                let v = self.val_op(h, f, c, p)?;
                let r = match &v {
                    V::Str(s) if !(s.len() > 2 && s[0] == b'0' && (s[1] | 0x20) == b'x') => {
                        let mut x = 0u64;
                        for &ch in s { if !ch.is_ascii_digit() { break; } x = x.wrapping_mul(10).wrapping_add((ch - b'0') as u64); }
                        x
                    }
                    o => self.to_int(o)?,
                };
                let t = self.target(h, f, c, p)?;
                self.store(h, f, &t, &V::Int(r))?;
                V::Int(r)
            }
            0x9C => { // ToString
                let v = self.val_op(h, f, c, p)?;
                let n = self.int_op(h, f, c, p)? as usize;
                let s: Vec<u8> = self.to_bytes(&v)?.into_iter().take_while(|&x| x != 0).take(n).collect();
                let r = V::Str(s);
                let t = self.target(h, f, c, p)?;
                self.store(h, f, &t, &r)?;
                r
            }
            0x9E => { // Mid
                let v = self.val_op(h, f, c, p)?;
                let i = self.int_op(h, f, c, p)? as usize;
                let n = self.int_op(h, f, c, p)? as usize;
                let b = self.to_bytes(&v)?;
                let lo = i.min(b.len());
                let hi = i.saturating_add(n).min(b.len());
                let s = b[lo..hi].to_vec();
                let r = if let V::Str(_) = v { V::Str(s) } else { V::buf(s) };
                let t = self.target(h, f, c, p)?;
                self.store(h, f, &t, &r)?;
                r
            }
            0x5B => {
                let op2 = *c.get(*p).ok_or(Err::Eof)?;
                *p += 1;
                match op2 {
                    0x12 => { // CondRefOf
                        let found = if c.get(*p).map_or(false, |&b| is_name_start(b)) {
                            let np = name_path(c, p)?;
                            self.lookup(f.scope, &np).filter(|&n| !matches!(self.nodes[n].obj, Obj::External { .. }))
                        } else {
                            match self.super_name(h, f, c, p)? { Tgt::Node(n) => Some(n), _ => None }
                        };
                        let t = self.target(h, f, c, p)?;
                        if let Some(n) = found {
                            if let Tgt::Local(l) = t { f.locals[l] = V::Node(n); }
                            V::Int(ones)
                        } else { V::Int(0) }
                    }
                    0x23 => { self.super_name(h, f, c, p)?; *p += 2; V::Int(0) } // Acquire: always succeeds
                    0x25 => { self.super_name(h, f, c, p)?; self.term(h, f, c, p)?; V::Int(0) } // Wait
                    0x28 | 0x29 => {
                        let a = self.int_op(h, f, c, p)?;
                        let r = if op2 == 0x28 { from_bcd(a) } else { to_bcd(a) };
                        let t = self.target(h, f, c, p)?;
                        self.store(h, f, &t, &V::Int(r))?;
                        V::Int(r)
                    }
                    0x30 => V::Int(0x2020_0101),
                    0x31 => V::Uninit,
                    0x33 => V::Int(h.timer()),
                    0x1F => { for _ in 0..6 { self.term(h, f, c, p)?; } V::Int(0) } // LoadTable: unsupported
                    _ => {
                        // A definition where a term is parsed: run it as one.
                        *p = at;
                        self.statement(h, f, c, p, false)?;
                        V::Uninit
                    }
                }
            }
            b if is_name_start(b) => {
                *p = at;
                let np = name_path(c, p)?;
                let n = self.lookup(f.scope, &np).ok_or(Err::Undefined(at))?;
                if let Some(argc) = self.method_argc(n) {
                    let mut args: Vec<V> = Vec::new();
                    for _ in 0..argc {
                        let v = self.term(h, f, c, p)?;
                        let v = match v { V::Node(x) if !self.is_device(x) && !self.is_method(x) => self.read_node(h, x)?, o => o };
                        args.push(v);
                    }
                    match self.nodes[n].obj.clone() {
                        Obj::Osi => {
                            let s = args.first().and_then(|a| a.as_bytes()).unwrap_or_default();
                            V::Int(if OSI_TRUE.iter().any(|&t| t == &s[..]) { ones } else { 0 })
                        }
                        Obj::Method { code, int32, .. } => self.invoke(h, n, code, int32, &args)?,
                        _ => V::Int(0),
                    }
                } else {
                    match &self.nodes[n].obj {
                        Obj::Name(_) | Obj::Field(_) | Obj::BufField { .. } => self.read_node(h, n)?,
                        _ => V::Node(n),
                    }
                }
            }
            _ => {
                let op2 = c.get(*p).copied().unwrap_or(0);
                return Err(Err::BadOpcode(op, op2, at));
            }
        })
    }

    fn cmp(&mut self, h: &mut dyn Host, f: &mut Frame, c: &'static [u8], p: &mut usize) -> R<Ord> {
        let a = self.val_op(h, f, c, p)?;
        let b = self.val_op(h, f, c, p)?;
        Ok(match a {
            V::Str(_) | V::Buf(_) => self.to_bytes(&a)?.cmp(&self.to_bytes(&b)?),
            _ => self.to_int(&a)?.cmp(&self.to_int(&b)?),
        })
    }
}

fn mop(op: u8, e: u64, v: u64) -> bool {
    match op { 0 => true, 1 => e == v, 2 => e <= v, 3 => e < v, 4 => e >= v, 5 => e > v, _ => false }
}

fn vtype(v: &V) -> u64 {
    match v { V::Uninit => 0, V::Int(_) => 1, V::Str(_) => 2, V::Buf(_) => 3, V::Pkg(_) => 4, _ => 0 }
}

fn from_bcd(mut a: u64) -> u64 { let mut r = 0; let mut m = 1; while a > 0 { r += (a & 0xF) * m; m *= 10; a >>= 4; } r }
fn to_bcd(mut a: u64) -> u64 { let mut r = 0; let mut s = 0; while a > 0 && s < 64 { r |= (a % 10) << s; a /= 10; s += 4; } r }

fn hex_str(i: u64, int32: bool) -> String {
    if int32 { alloc::format!("{:08X}", i as u32) } else { alloc::format!("{:016X}", i) }
}

fn np_str(np: &NamePath) -> Vec<u8> {
    let mut s = Vec::new();
    if np.root { s.push(b'\\'); }
    for _ in 0..np.up { s.push(b'^'); }
    if np.segs.len() == 2 { s.push(0x2E); } else if np.segs.len() > 2 { s.push(0x2F); s.push(np.segs.len() as u8); }
    for sg in &np.segs { s.extend_from_slice(sg); }
    s
}

fn rd_le(c: &[u8], p: usize, n: usize) -> R<u64> {
    if p + n > c.len() { return Err(Err::Eof); }
    let mut v = 0u64;
    for i in 0..n { v |= (c[p + i] as u64) << (8 * i); }
    Ok(v)
}

fn extract_bits(b: &[u8], off: usize, bits: usize) -> Vec<u8> {
    let mut out = vec![0u8; bits.div_ceil(8)];
    for i in 0..bits {
        let s = off + i;
        if s / 8 >= b.len() { break; }
        if (b[s / 8] >> (s % 8)) & 1 != 0 { out[i / 8] |= 1 << (i % 8); }
    }
    out
}

/// A field's bytes as a value: an Integer when it fits, else a Buffer.
fn bytes_value(out: Vec<u8>, bits: usize) -> V {
    if bits <= 64 {
        let mut x = 0u64;
        for (i, &b) in out.iter().enumerate() { x |= (b as u64) << (8 * i); }
        V::Int(x)
    } else {
        V::buf(out)
    }
}

/// EISA id (`PNP0C0A`) in the compressed 32-bit form `_HID` returns.
pub fn eisa_id(s: &str) -> u64 {
    let b = s.as_bytes();
    if b.len() != 7 { return 0; }
    let c = |x: u8| ((x.wrapping_sub(0x40)) & 0x1F) as u32;
    let hx = u32::from_str_radix(&s[3..], 16).unwrap_or(0);
    let v = (c(b[0]) << 26) | (c(b[1]) << 21) | (c(b[2]) << 16) | hx;
    v.swap_bytes() as u64
}
