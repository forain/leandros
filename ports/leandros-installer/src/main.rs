// LeandrOS installer GUI. Unprivileged; all disk work is done by the
// `leandros-install` backend (see backend.rs).
use cosmic::app::{Core, Settings, Task};
use cosmic::iced::{Alignment, Length, Limits};
use cosmic::{Application, Element, theme, widget};
use futures::StreamExt;

mod backend;
use backend::{Disk, Event, Mode, Plan};

const GIB: u64 = 1 << 30;
/// Safety margin added on top of the filesystem minimum when shrinking.
const SHRINK_MARGIN_GIB: u64 = 2;
/// boot 512 MiB + log 64 MiB + home >= 2 GiB (root is chosen separately).
const FIXED_OVERHEAD: u64 = (512 << 20) + (64 << 20) + 2 * GIB;
const MIN_ROOT_GIB: u64 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Welcome,
    Disk,
    Mode,
    Size,
    Confirm,
    Progress,
    Done,
    ShrinkDone,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    List,
    Dry,
    Install,
    Shrink,
    ShrinkStatus,
    Recover,
}

/// Slider bounds (GiB) for the Size page: (lo, hi, is_shrink).
fn bounds(d: &Disk, mode: Mode) -> (u64, u64) {
    match mode {
        Mode::Free => (MIN_ROOT_GIB, d.gap_bytes.saturating_sub(FIXED_OVERHEAD) / GIB),
        Mode::Shrink => {
            let lo = d.shrink_min.div_ceil(GIB) + SHRINK_MARGIN_GIB;
            // At least 19 GiB must be freed for the install.
            (lo, (d.shrink_cur / GIB).saturating_sub(19))
        }
        Mode::Erase => (MIN_ROOT_GIB, MIN_ROOT_GIB),
    }
}

#[derive(Clone, Debug)]
enum Message {
    Next,
    Back,
    Password(String),
    Select(usize),
    SetMode(Mode),
    Slider(u32),
    RootText(String),
    Typed(String),
    TypedErase(String),
    Backup(bool),
    Ack(bool),
    Backend(Event),
    PowerOff,
    Recover,
}

struct App {
    core: Core,
    page: Page,
    password: String,
    disks: Vec<Disk>,
    selected: Option<usize>,
    typed: String,
    typed_erase: String,
    backed_up: bool,
    mode: Mode,
    /// Root size (free/erase) or new Linux size (shrink), in GiB.
    slider: u64,
    root_text: String,
    op: Op,
    gates: Vec<(String, String, String, String)>,
    plan: Option<Plan>,
    dry_ok: bool,
    steps: Vec<(String, u32)>,
    ack: bool,
    loading: bool,
    error: String,
    step: String,
    pct: u32,
    installing: bool,
    finished: bool,
    /// Phase of an interrupted shrink found on startup (None = nothing to recover).
    interrupted: Option<String>,
}

fn log(msg: &str) {
    eprintln!("leandros-installer: {msg}");
}

fn start(args: Vec<String>, password: Option<String>) -> Task<Message> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    backend::run(args, password, tx);
    cosmic::task::stream(rx.map(|e| cosmic::Action::App(Message::Backend(e))))
}

impl App {
    fn pw(&self) -> Option<String> {
        (!backend::is_root()).then(|| self.password.clone())
    }

    fn disk(&self) -> Option<Disk> {
        self.selected.and_then(|i| self.disks.get(i)).cloned()
    }

    fn gates_view(&self) -> Element<'_, Message> {
        let mut c = widget::Column::new();
        for (id, name, status, msg) in &self.gates {
            let tag = match status.as_str() {
                "pass" => "[pass]",
                "fail" => "[FAIL]",
                _ => "[n/a] ",
            };
            c = c.push(widget::text::caption(format!("{tag} {id} {name} {msg}")));
        }
        c.into()
    }

    fn root_gib(&self) -> Option<u64> {
        (self.mode != Mode::Shrink && self.mode != Mode::Erase).then_some(self.slider)
    }

    fn begin(&mut self, op: Op, args: Vec<String>) -> Task<Message> {
        self.op = op;
        self.error.clear();
        self.finished = false;
        self.gates.clear();
        self.steps.clear();
        self.step = "starting".into();
        self.pct = 0;
        if op == Op::Dry {
            self.plan = None;
            self.dry_ok = false;
        }
        start(args, self.pw())
    }

    fn check_shrink(&mut self) -> Task<Message> {
        self.error.clear();
        self.loading = true;
        self.op = Op::ShrinkStatus;
        start(backend::shrink_status_args(), self.pw())
    }

    fn load_disks(&mut self) -> Task<Message> {
        self.disks.clear();
        self.selected = None;
        self.error.clear();
        self.loading = true;
        self.op = Op::List;
        start(backend::list_args(), self.pw())
    }
}

impl Application for App {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = "com.leandros.Installer";

    fn core(&self) -> &Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(mut core: Core, _: ()) -> (Self, Task<Message>) {
        core.window.show_close = false;
        log("window init");
        (
            App {
                core,
                page: Page::Welcome,
                password: String::new(),
                disks: vec![],
                selected: None,
                typed: String::new(),
                typed_erase: String::new(),
                backed_up: false,
                mode: Mode::Free,
                slider: MIN_ROOT_GIB,
                root_text: String::new(),
                op: Op::List,
                gates: vec![],
                plan: None,
                dry_ok: false,
                steps: vec![],
                ack: false,
                loading: false,
                error: String::new(),
                step: String::new(),
                pct: 0,
                installing: false,
                finished: false,
                interrupted: None,
            },
            if backend::is_root() { start(backend::shrink_status_args(), None) } else { Task::none() },
        )
    }

    fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::Password(p) => self.password = p,
            Message::Select(i) => {
                if self.disks.get(i).is_some_and(|d| d.eligible) {
                    self.selected = Some(i);
                }
            }
            Message::Typed(t) => self.typed = t,
            Message::TypedErase(t) => self.typed_erase = t,
            Message::Backup(a) => self.backed_up = a,
            Message::Ack(a) => self.ack = a,
            Message::SetMode(m) => self.mode = m,
            Message::Slider(v) => {
                self.slider = v as u64;
                self.root_text = v.to_string();
            }
            Message::RootText(t) => {
                if let (Ok(v), Some(d)) = (t.trim().parse::<u64>(), self.disk()) {
                    let (lo, hi) = bounds(&d, self.mode);
                    self.slider = v.clamp(lo, hi.max(lo));
                }
                self.root_text = t;
            }
            Message::Back => {
                self.error.clear();
                self.page = match self.page {
                    Page::Disk => Page::Welcome,
                    Page::Mode => Page::Disk,
                    Page::Size => Page::Mode,
                    Page::Confirm if self.mode == Mode::Erase => Page::Mode,
                    Page::Confirm => Page::Size,
                    Page::Progress if !self.installing && self.op == Op::Recover => Page::Welcome,
                    Page::Progress if !self.installing => Page::Mode,
                    p => p,
                };
            }
            Message::Next => match self.page {
                Page::Welcome => return self.check_shrink(),
                Page::Disk if self.selected.is_some() => {
                    self.mode = match self.disk() {
                        Some(d) if d.dualboot => Mode::Free,
                        Some(d) if !d.shrink_part.is_empty() => Mode::Shrink,
                        _ => Mode::Erase,
                    };
                    self.page = Page::Mode;
                }
                Page::Mode => {
                    self.typed.clear();
                    self.typed_erase.clear();
                    self.ack = false;
                    self.backed_up = false;
                    if self.mode == Mode::Erase {
                        self.page = Page::Confirm;
                    } else if let Some(d) = self.disk() {
                        let (lo, hi) = bounds(&d, self.mode);
                        self.slider = if self.mode == Mode::Shrink { hi.max(lo) } else { MIN_ROOT_GIB.clamp(lo, hi.max(lo)) };
                        self.root_text = self.slider.to_string();
                        self.page = Page::Size;
                    }
                }
                Page::Size => {
                    self.page = Page::Confirm;
                    if self.mode == Mode::Free {
                        if let Some(d) = self.disk() {
                            let a = backend::install_args(&d.dev, Mode::Free, self.root_gib(), true);
                            return self.begin(Op::Dry, a);
                        }
                    }
                }
                Page::Confirm => {
                    let Some(d) = self.disk() else {
                        return Task::none();
                    };
                    self.page = Page::Progress;
                    self.installing = true;
                    log(&format!("start {:?} {}", self.mode, d.dev));
                    if self.mode == Mode::Shrink {
                        let a = backend::shrink_args(&d.shrink_part, self.slider * GIB);
                        return self.begin(Op::Shrink, a);
                    }
                    let a = backend::install_args(&d.dev, self.mode, self.root_gib(), false);
                    return self.begin(Op::Install, a);
                }
                _ => {}
            },
            Message::Backend(ev) => match ev {
                Event::Disk(d) => self.disks.push(d),
                Event::Gate { id, name, status, msg } => {
                    log(&format!("gate {id} {status}"));
                    self.gates.push((id, name, status, msg));
                }
                Event::Plan(p) => self.plan = Some(p),
                Event::Info(_) => {}
                Event::ShrinkStatus { state, phase } => {
                    log(&format!("shrink-status {state} {phase}"));
                    self.interrupted = (state == "interrupted").then_some(phase);
                }
                Event::Step { name, pct } => {
                    log(&format!("step {name} {pct}"));
                    match self.steps.iter_mut().find(|(n, _)| *n == name) {
                        Some(e) => e.1 = pct,
                        None => self.steps.push((name.clone(), pct)),
                    }
                    self.step = name;
                    self.pct = pct;
                }
                Event::Done { root_partuuid, dry_run, .. } => {
                    log(&format!("done {root_partuuid} dry={dry_run}"));
                    if dry_run {
                        self.dry_ok = self.plan.is_some() && !self.gates.iter().any(|g| g.2 == "fail");
                    } else {
                        self.finished = true;
                    }
                }
                Event::Error(m) => self.error = m,
                Event::Exit(code, stderr) => match self.op {
                    Op::Install | Op::Shrink => {
                        self.installing = false;
                        if self.finished {
                            self.page = if self.op == Op::Shrink { Page::ShrinkDone } else { Page::Done };
                        } else if self.error.is_empty() {
                            self.error = format!("Installer exited ({code:?}) {}", stderr.trim());
                        }
                    }
                    Op::Dry => {
                        if !self.dry_ok && self.error.is_empty() {
                            self.error = if self.gates.iter().any(|g| g.2 == "fail") {
                                "A safety check failed; nothing was written.".into()
                            } else {
                                format!("Dry run failed ({code:?}) {}", stderr.trim())
                            };
                        }
                    }
                    Op::ShrinkStatus => {
                        if backend::wrong_password(&stderr) {
                            self.loading = false;
                            self.error = "Incorrect password.".into();
                        } else if self.interrupted.is_some() {
                            self.loading = false;
                        } else {
                            return self.load_disks();
                        }
                    }
                    Op::Recover => {
                        self.installing = false;
                        if self.finished {
                            self.interrupted = None;
                            self.error.clear();
                            self.page = Page::Welcome;
                        } else if self.error.is_empty() {
                            self.error = format!("Recovery exited ({code:?}) {}", stderr.trim());
                        }
                    }
                    Op::List => {
                        self.loading = false;
                        if backend::wrong_password(&stderr) {
                            self.error = "Incorrect password.".into();
                        } else if code != Some(0) && self.error.is_empty() {
                            self.error = format!("Could not list disks ({code:?}) {}", stderr.trim());
                        } else if self.error.is_empty() {
                            self.page = Page::Disk;
                        }
                    }
                },
            },
            Message::Recover => {
                self.mode = Mode::Shrink;
                self.page = Page::Progress;
                self.installing = true;
                return self.begin(Op::Recover, backend::recover_args());
            }
            Message::PowerOff => {
                let _ = std::process::Command::new("poweroff").spawn();
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let sp = theme::spacing();
        let mut col = widget::Column::new().spacing(sp.space_s).padding(sp.space_l).width(Length::Fill);
        let nav = |back: bool, next: Option<(&'static str, bool)>| {
            let mut r = widget::Row::new().spacing(sp.space_s).align_y(Alignment::Center);
            if back {
                r = r.push(widget::button::standard("Back").on_press(Message::Back));
            }
            r = r.push(widget::space::horizontal());
            if let Some((l, en)) = next {
                let b = widget::button::suggested(l);
                r = r.push(b.on_press_maybe(en.then_some(Message::Next)));
            }
            r
        };
        match self.page {
            Page::Welcome if self.interrupted.is_some() => {
                col = col
                    .push(widget::text::title1("A previous shrink was interrupted"))
                    .push(widget::text::body(format!(
                        "The last attempt to shrink your Linux partition did not finish (stopped in phase \"{}\"), \
                         for example because of a power loss. Your Linux partition may be unusable until it is \
                         recovered. Recover restores it from the undo data saved on this USB stick. Do not run \
                         fsck, GParted or other disk tools on it first. Installing and shrinking are disabled until recovery succeeds.",
                        self.interrupted.clone().unwrap_or_default()
                    )));
                if !backend::is_root() {
                    col = col
                        .push(widget::text::body("Enter your password to allow disk access:"))
                        .push(widget::secure_input("Password", &self.password, None, true).on_input(Message::Password));
                }
                if !self.error.is_empty() {
                    col = col.push(widget::text::body(self.error.clone()));
                }
                col = col.push(widget::button::destructive("Recover").on_press(Message::Recover));
            }
            Page::Welcome => {
                col = col
                    .push(widget::text::title1("Install LeandrOS"))
                    .push(widget::text::body(
                        "This will install LeandrOS onto a disk in this computer, next to your \
                         existing system when there is room, or by erasing the disk.",
                    ));
                if !backend::is_root() {
                    col = col
                        .push(widget::text::body("Enter your password to allow disk access:"))
                        .push(
                            widget::secure_input("Password", &self.password, None, true)
                                .on_input(Message::Password)
                                .on_submit(|_| Message::Next),
                        );
                }
                if self.loading {
                    col = col.push(widget::text::body("Looking for disks..."));
                }
                if !self.error.is_empty() {
                    col = col.push(widget::text::body(self.error.clone()));
                }
                col = col.push(nav(false, Some(("Continue", !self.loading))));
            }
            Page::Disk => {
                col = col.push(widget::text::title2("Choose the disk to install to"));
                for (i, d) in self.disks.iter().enumerate() {
                    let mut line = format!("{}  -  {}", d.label(), d.size_human());
                    if !d.eligible {
                        line.push_str(&format!("  (unavailable: {})", d.reason));
                    }
                    let txt = widget::text::body(line);
                    let sel = self.selected == Some(i);
                    let b = widget::button::custom(txt)
                        .class(if sel { theme::Button::Suggested } else { theme::Button::Standard })
                        .width(Length::Fill)
                        .on_press_maybe(d.eligible.then_some(Message::Select(i)));
                    col = col.push(b);
                }
                if self.disks.is_empty() {
                    col = col.push(widget::text::body("No disks found."));
                }
                col = col.push(nav(true, Some(("Next", self.selected.is_some()))));
            }
            Page::Mode => {
                let d = self.disk().unwrap_or_default();
                col = col.push(widget::text::title2(format!("How should LeandrOS be installed on {}?", d.label())));
                let opt = |m: Mode, title: &str, on: bool| {
                    let sel = self.mode == m;
                    widget::button::custom(widget::text::body(title.to_string()))
                        .class(if sel { theme::Button::Suggested } else { theme::Button::Standard })
                        .width(Length::Fill)
                        .on_press_maybe(on.then_some(Message::SetMode(m)))
                };
                col = col.push(opt(Mode::Free, "Install alongside (use free space)", d.dualboot));
                if d.dualboot {
                    col = col.push(widget::text::caption(
                        "Your existing system is not touched. LeandrOS goes into unallocated space.",
                    ));
                } else {
                    col = col.push(widget::text::caption(format!("Not available: {}", d.dualboot_reason)));
                }
                let can_shrink = !d.shrink_part.is_empty();
                col = col.push(opt(Mode::Shrink, "Shrink Linux to make room", can_shrink));
                if can_shrink {
                    col = col.push(widget::text::caption(
                        "Two sessions: this session shrinks Linux; boot Linux once to check it; \
                         then come back and install.",
                    ));
                } else {
                    col = col.push(widget::text::caption("Not available: no shrinkable Linux partition was found."));
                }
                col = col.push(opt(Mode::Erase, "Erase disk", true));
                if self.mode == Mode::Erase {
                    col = col.push(widget::text::title3(format!("WARNING: everything on {} will be destroyed.", d.dev)));
                }
                col = col.push(nav(true, Some(("Next", true))));
            }
            Page::Size => {
                let d = self.disk().unwrap_or_default();
                let (lo, hi) = bounds(&d, self.mode);
                col = col.push(widget::text::title2(if self.mode == Mode::Shrink {
                    "How large should Linux stay?"
                } else {
                    "How large should the LeandrOS root be?"
                }));
                if hi < lo {
                    col = col.push(widget::text::body("There is not enough room on this disk."));
                } else {
                    col = col
                        .push(widget::text::body(format!("{} GiB   (minimum {lo} GiB, maximum {hi} GiB)", self.slider)))
                        .push(widget::slider(lo as u32..=hi as u32, self.slider as u32, Message::Slider))
                        .push(widget::text_input("GiB", &self.root_text).on_input(Message::RootText));
                    if self.mode == Mode::Shrink {
                        col = col.push(widget::text::caption(format!(
                            "Linux is {} GiB now; its minimum is {} GiB plus a {} GiB margin. About {} GiB is freed.",
                            d.shrink_cur / GIB,
                            d.shrink_min.div_ceil(GIB),
                            SHRINK_MARGIN_GIB,
                            (d.shrink_cur / GIB).saturating_sub(self.slider)
                        )));
                    }
                }
                col = col.push(nav(true, Some(("Next", hi >= lo))));
            }
            Page::Confirm => {
                let d = self.disk().unwrap_or_default();
                col = col.push(widget::text::title2("Confirm"));
                col = col.push(widget::text::body(format!("{}, {}", d.label(), d.size_human())));
                match self.mode {
                    Mode::Erase => {
                        col = col.push(widget::text::title3(format!("ALL DATA ON {} WILL BE ERASED", d.dev)));
                    }
                    Mode::Shrink => {
                        col = col.push(widget::text::body(format!(
                            "This session shrinks {} to {} GiB. Then boot Linux once to check it and come back to install.",
                            d.shrink_part, self.slider
                        )));
                        col = col.push(widget::text::title3("Keep the laptop plugged in and do not close the lid."));
                        col = col.push(widget::text::body(
                            "If the power fails or the laptop restarts during the resize, your Linux system will not start: it will stop at an emergency or maintenance prompt. This is deliberate, so that Linux cannot damage the half-resized disk.",
                        ));
                        col = col.push(widget::text::body(
                            "If that happens: switch the laptop off, keep this USB stick plugged in, open the boot menu and boot the stick again, then choose Recover. Do not run fsck, GParted or other disk tools on the Linux partition before that.",
                        ));
                    }
                    Mode::Free => {
                        if self.dry_ok {
                            col = col.push(widget::text::body("Dry run passed. Nothing has been written yet."));
                        } else if self.error.is_empty() {
                            col = col.push(widget::text::body("Checking (dry run)..."));
                        } else {
                            col = col.push(widget::text::title3(self.error.clone()));
                        }
                        col = col.push(self.gates_view());
                        if let Some(p) = &self.plan {
                            col = col.push(widget::text::title3("Partition table"));
                            col = col.push(widget::text::body("Before:"));
                            for r in &p.old_gpt {
                                col = col.push(widget::text::caption(format!("  p{}  {} MiB  {}", r.n, r.mib(), r.name)));
                            }
                            col = col.push(widget::text::body("After:"));
                            for r in &p.new_gpt {
                                let new = !p.old_gpt.iter().any(|o| o.n == r.n);
                                col = col.push(widget::text::caption(format!(
                                    "  p{}  {} MiB  {}{}",
                                    r.n,
                                    r.mib(),
                                    r.name,
                                    if new { "   (new)" } else { "" }
                                )));
                            }
                            col = col.push(widget::text::body("Files added to the shared boot partition (ESP):"));
                            for (path, bytes) in &p.esp_files {
                                col = col.push(widget::text::caption(format!("  {path}  ({bytes} bytes)")));
                            }
                        }
                    }
                }
                col = col
                    .push(widget::text::body("Type the disk name to confirm:"))
                    .push(widget::text_input(d.dev.clone(), &self.typed).on_input(Message::Typed));
                if self.mode == Mode::Erase {
                    col = col
                        .push(widget::text::body("This erases the whole disk. Type ERASE to continue:"))
                        .push(widget::text_input("ERASE", &self.typed_erase).on_input(Message::TypedErase));
                }
                col = col
                    .push(widget::checkbox(self.backed_up).label("I have backed up my data").on_toggle(Message::Backup))
                    .push(widget::checkbox(self.ack).label("I understand this cannot be undone").on_toggle(Message::Ack));
                let mut ok = self.ack && self.backed_up && self.typed.trim() == d.dev;
                ok &= match self.mode {
                    Mode::Erase => self.typed_erase.trim() == "ERASE",
                    Mode::Free => self.dry_ok,
                    Mode::Shrink => true,
                };
                let label = match self.mode {
                    Mode::Shrink => "Shrink Linux",
                    Mode::Free => "Install",
                    Mode::Erase => "Erase and install",
                };
                col = col.push(nav(true, Some((label, ok))));
            }
            Page::Progress => {
                col = col
                    .push(widget::text::title2(if self.mode == Mode::Shrink { "Shrinking Linux" } else { "Installing" }))
                    .push(widget::text::body(format!("Step: {} ({}%)", self.step, self.pct)))
                    .push(widget::progress_bar::determinate_linear(self.pct as f32 / 100.0))
                    .push(self.gates_view());
                for (n, p) in &self.steps {
                    col = col.push(widget::text::caption(format!("  {n}: {}", if *p >= 100 { "done".to_string() } else { format!("{p}%") })));
                }
                col = col.push(widget::text::body(if self.mode == Mode::Erase {
                    "The system is briefly read-only while the root volume is copied. Do not power off."
                } else {
                    "Do not power off."
                }));
                if !self.error.is_empty() {
                    col = col.push(widget::text::title3(format!("Failed: {}", self.error)));
                    if !self.installing {
                        col = col.push(nav(true, None));
                    }
                }
            }
            Page::ShrinkDone => {
                col = col
                    .push(widget::text::title2("Linux has been shrunk"))
                    .push(widget::text::body(
                        "Session 1 is complete. Now restart into Linux and check that it boots and works. \
                         Then boot this installer again and choose \"Install alongside\" to finish.",
                    ))
                    .push(widget::button::destructive("Power off").on_press(Message::PowerOff));
            }
            Page::Done => {
                col = col.push(widget::text::title2("Installation complete"));
                if self.mode == Mode::Erase {
                    col = col.push(widget::text::body("Shut down, remove the USB stick, then power on."));
                } else {
                    col = col.push(widget::text::body(
                        "Reboot and choose LeandrOS in the boot menu; Linux stays the default.",
                    ));
                }
                col = col.push(widget::button::destructive("Power off").on_press(Message::PowerOff));
            }
        }
        widget::container(widget::scrollable(col)).width(Length::Fill).height(Length::Fill).into()
    }
}

fn self_test_list() -> i32 {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    backend::run(backend::list_args(), std::env::var("LEANDROS_INSTALL_PASSWORD").ok(), tx);
    futures::executor::block_on(async {
        println!("DEV\tSIZE\tELIGIBLE\tMODEL\tREASON");
        while let Some(ev) = rx.next().await {
            match ev {
                Event::Disk(d) => println!("{}\t{}\t{}\t{}\t{}", d.dev, d.size_human(), d.eligible, d.model, d.reason),
                Event::Exit(code, err) => {
                    if !err.trim().is_empty() {
                        eprintln!("{}", err.trim());
                    }
                    println!("exit {:?}", code);
                    return if code == Some(0) { 0 } else { 1 };
                }
                _ => {}
            }
        }
        1
    })
}

/// Run one backend command headless through the same event handling as the GUI.
/// Returns (done, dry_done, gate_failed, exit code).
fn drain(args: Vec<String>, label: &str) -> (bool, bool, bool, Option<i32>) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    backend::run(args, std::env::var("LEANDROS_INSTALL_PASSWORD").ok(), tx);
    futures::executor::block_on(async {
        let (mut done, mut dry, mut gfail, mut last) = (false, false, false, String::new());
        let mut plan = false;
        while let Some(ev) = rx.next().await {
            match ev {
                Event::Gate { id, name, status, msg } => {
                    gfail |= status == "fail";
                    println!("gate {id} {name} {status} {msg}");
                }
                Event::Plan(p) => {
                    plan = true;
                    println!("plan old={} new={} esp_files={}", p.old_gpt.len(), p.new_gpt.len(), p.esp_files.len());
                    for r in &p.new_gpt {
                        println!("  new p{} {} MiB {}", r.n, r.mib(), r.name);
                    }
                    for (f, b) in &p.esp_files {
                        println!("  esp {f} {b}");
                    }
                }
                Event::Step { name, pct } => {
                    if name != last {
                        println!("step {name} -> page Progress ({label})");
                        last = name;
                    }
                    if pct == 100 {
                        println!("  {last} 100");
                    }
                }
                Event::Done { root_partuuid, disk_guid, dry_run } => {
                    if dry_run {
                        dry = plan;
                        println!("dry-run done");
                    } else {
                        done = true;
                        println!("done root_partuuid={root_partuuid} disk_guid={disk_guid}");
                    }
                }
                Event::Error(m) => println!("error {m}"),
                Event::ShrinkStatus { state, phase } => println!("shrink-status {state} {phase}"),
                Event::Exit(code, err) => {
                    if !err.trim().is_empty() {
                        eprintln!("{}", err.trim());
                    }
                    return (done, dry, gfail, code);
                }
                _ => {}
            }
        }
        (done, dry, gfail, None)
    })
}

fn self_test_dualboot(dev: &str) -> i32 {
    println!("page Confirm: dry run");
    let (_, dry, gfail, _) = drain(backend::install_args(dev, Mode::Free, None, true), "dry");
    if gfail || !dry {
        println!("dry run -> page Confirm (blocked: gate failed or no plan)");
        return 1;
    }
    println!("dry run ok -> Install enabled");
    let (done, _, gfail, code) = drain(backend::install_args(dev, Mode::Free, None, false), "install");
    println!("exit {:?} -> page {}", code, if done { "Done" } else { "Error" });
    if done && !gfail && code == Some(0) { 0 } else { 1 }
}

fn parse_size(s: &str) -> u64 {
    let (n, mul) = match s.chars().last() {
        Some('G') | Some('g') => (&s[..s.len() - 1], GIB),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1 << 20),
        _ => (s, 1),
    };
    n.parse::<u64>().unwrap_or(0) * mul
}

fn self_test_shrink(dev: &str, size: &str) -> i32 {
    let to = parse_size(size);
    if to == 0 {
        println!("bad size {size}");
        return 2;
    }
    // Find the shrinkable partition the list reports for this disk; else dev is the partition.
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    backend::run(backend::list_args(), std::env::var("LEANDROS_INSTALL_PASSWORD").ok(), tx);
    let part = futures::executor::block_on(async {
        let mut part = dev.to_string();
        while let Some(ev) = rx.next().await {
            match ev {
                Event::Disk(d) if d.dev == dev && !d.shrink_part.is_empty() => part = d.shrink_part,
                Event::Exit(..) => break,
                _ => {}
            }
        }
        part
    });
    println!("page Confirm -> shrink {part} to {to} bytes");
    let (done, _, gfail, code) = drain(backend::shrink_args(&part, to), "shrink");
    println!("exit {:?} -> page {}", code, if done { "ShrinkDone" } else { "Error" });
    let (_, _, _, sc) = drain(backend::shrink_status_args(), "status");
    println!("shrink-status exit {:?}", sc);
    if done && !gfail && code == Some(0) { 0 } else { 1 }
}

/// Startup flow of the GUI: shrink-status; if interrupted show the Recover banner and
/// run the recovery (as the Recover button does); the status must then be clear.
fn self_test_recover() -> i32 {
    let st = |label: &str| -> Option<String> {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        backend::run(backend::shrink_status_args(), std::env::var("LEANDROS_INSTALL_PASSWORD").ok(), tx);
        let r = futures::executor::block_on(async {
            let mut r = None;
            while let Some(ev) = rx.next().await {
                match ev {
                    Event::ShrinkStatus { state, phase } => {
                        println!("shrink-status {state} {phase} ({label})");
                        r = Some(if state == "interrupted" { phase } else { String::new() });
                    }
                    Event::Exit(..) => break,
                    _ => {}
                }
            }
            r
        });
        r
    };
    match st("startup") {
        Some(p) if !p.is_empty() => println!("page Welcome: banner interrupted phase={p}; Install/Shrink hidden; Recover enabled"),
        _ => {
            println!("no interrupted shrink");
            return 1;
        }
    }
    let (done, _, _, code) = drain(backend::recover_args(), "recover");
    println!("exit {:?} -> page {}", code, if done { "Welcome (recovered)" } else { "Error" });
    if !done || code != Some(0) {
        return 1;
    }
    match st("after") {
        Some(p) if p.is_empty() => {
            println!("banner cleared; Install/Shrink modes available");
            0
        }
        _ => 1,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--self-test-list") => std::process::exit(self_test_list()),
        Some("--self-test-install") => {
            let dev = args.get(1).map(String::as_str).unwrap_or("/dev/fake");
            let (done, _, _, code) = drain(backend::install_args(dev, Mode::Erase, None, false), "install");
            println!("exit {:?} -> page {}", code, if done { "Done" } else { "Error" });
            std::process::exit(if done && code == Some(0) { 0 } else { 1 });
        }
        Some("--self-test-dualboot") => {
            let dev = args.get(1).map(String::as_str).unwrap_or("/dev/nvme0n1");
            std::process::exit(self_test_dualboot(dev));
        }
        Some("--self-test-recover") => std::process::exit(self_test_recover()),
        Some("--self-test-shrink") => {
            let dev = args.get(1).map(String::as_str).unwrap_or("/dev/nvme0n1");
            let size = args.get(2).map(String::as_str).unwrap_or("30G");
            std::process::exit(self_test_shrink(dev, size));
        }
        _ => {}
    }
    let settings = Settings::default().size_limits(Limits::NONE.min_width(560.0).min_height(420.0));
    cosmic::app::run::<App>(settings, ())?;
    Ok(())
}
