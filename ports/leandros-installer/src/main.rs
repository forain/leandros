// LeandrOS installer GUI. Unprivileged; all disk work is done by the
// `leandros-install` backend (see backend.rs).
use cosmic::app::{Core, Settings, Task};
use cosmic::iced::{Alignment, Length, Limits};
use cosmic::{Application, Element, theme, widget};
use futures::StreamExt;

mod backend;
use backend::{Disk, Event};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Welcome,
    Disk,
    Confirm,
    Progress,
    Done,
}

#[derive(Clone, Debug)]
enum Message {
    Next,
    Back,
    Password(String),
    Select(usize),
    Typed(String),
    Ack(bool),
    Backend(Event),
    PowerOff,
}

struct App {
    core: Core,
    page: Page,
    password: String,
    disks: Vec<Disk>,
    selected: Option<usize>,
    typed: String,
    ack: bool,
    loading: bool,
    error: String,
    step: String,
    pct: u32,
    installing: bool,
    finished: bool,
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

    fn load_disks(&mut self) -> Task<Message> {
        self.disks.clear();
        self.selected = None;
        self.error.clear();
        self.loading = true;
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
                ack: false,
                loading: false,
                error: String::new(),
                step: String::new(),
                pct: 0,
                installing: false,
                finished: false,
            },
            Task::none(),
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
            Message::Ack(a) => self.ack = a,
            Message::Back => {
                self.error.clear();
                self.page = match self.page {
                    Page::Disk => Page::Welcome,
                    Page::Confirm => Page::Disk,
                    p => p,
                };
            }
            Message::Next => match self.page {
                Page::Welcome => return self.load_disks(),
                Page::Disk if self.selected.is_some() => {
                    self.typed.clear();
                    self.ack = false;
                    self.page = Page::Confirm;
                }
                Page::Confirm => {
                    let Some(d) = self.selected.and_then(|i| self.disks.get(i)) else {
                        return Task::none();
                    };
                    self.page = Page::Progress;
                    self.installing = true;
                    self.error.clear();
                    self.step = "starting".into();
                    self.pct = 0;
                    log(&format!("install start {}", d.dev));
                    return start(backend::install_args(&d.dev), self.pw());
                }
                _ => {}
            },
            Message::Backend(ev) => match ev {
                Event::Disk(d) => self.disks.push(d),
                Event::Step { name, pct } => {
                    log(&format!("step {name} {pct}"));
                    self.step = name;
                    self.pct = pct;
                }
                Event::Done { root_partuuid } => {
                    log(&format!("done {root_partuuid}"));
                    self.finished = true;
                }
                Event::Error(m) => self.error = m,
                Event::Exit(code, stderr) => {
                    if self.installing {
                        self.installing = false;
                        if self.finished {
                            self.page = Page::Done;
                        } else if self.error.is_empty() {
                            self.error = format!("Installer exited ({code:?}) {}", stderr.trim());
                        }
                    } else {
                        self.loading = false;
                        if backend::wrong_password(&stderr) {
                            self.error = "Incorrect password.".into();
                        } else if code != Some(0) && self.error.is_empty() {
                            self.error = format!("Could not list disks ({code:?}) {}", stderr.trim());
                        } else if self.error.is_empty() {
                            self.page = Page::Disk;
                        }
                    }
                }
            },
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
            Page::Welcome => {
                col = col
                    .push(widget::text::title1("Install LeandrOS"))
                    .push(widget::text::body(
                        "This will install LeandrOS onto a disk in this computer. \
                         The selected disk is erased completely.",
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
            Page::Confirm => {
                let d = self.selected.and_then(|i| self.disks.get(i)).cloned().unwrap_or_default();
                col = col
                    .push(widget::text::title2("Confirm"))
                    .push(widget::text::title3(format!("ALL DATA ON {} WILL BE ERASED", d.dev)))
                    .push(widget::text::body(format!("{}, {}", d.label(), d.size_human())))
                    .push(widget::text::body("Type the disk name to confirm:"))
                    .push(widget::text_input(d.dev.clone(), &self.typed).on_input(Message::Typed))
                    .push(widget::checkbox(self.ack).label("I understand this cannot be undone").on_toggle(Message::Ack));
                let ok = self.ack && self.typed.trim() == d.dev;
                col = col.push(nav(true, Some(("Install", ok))));
            }
            Page::Progress => {
                col = col
                    .push(widget::text::title2("Installing"))
                    .push(widget::text::body(format!("Step: {} ({}%)", self.step, self.pct)))
                    .push(widget::progress_bar::determinate_linear(self.pct as f32 / 100.0))
                    .push(widget::text::body(
                        "The system is briefly read-only while the root volume is copied. \
                         Do not power off.",
                    ));
                if !self.error.is_empty() {
                    col = col.push(widget::text::title3(format!("Failed: {}", self.error)));
                }
            }
            Page::Done => {
                col = col
                    .push(widget::text::title2("Installation complete"))
                    .push(widget::text::body("Shut down, remove the USB stick, then power on."))
                    .push(widget::button::destructive("Power off").on_press(Message::PowerOff));
            }
        }
        widget::container(col).width(Length::Fill).height(Length::Fill).into()
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

fn self_test_install(dev: &str) -> i32 {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    backend::run(backend::install_args(dev), std::env::var("LEANDROS_INSTALL_PASSWORD").ok(), tx);
    futures::executor::block_on(async {
        let mut last = String::new();
        let mut done = false;
        while let Some(ev) = rx.next().await {
            match ev {
                Event::Step { name, pct } => {
                    if name != last {
                        println!("step {name} -> page Progress");
                        last = name;
                    }
                    println!("  pct {pct}");
                }
                Event::Done { root_partuuid } => {
                    done = true;
                    println!("done root_partuuid={root_partuuid}");
                }
                Event::Error(m) => println!("error {m}"),
                Event::Exit(code, _) => {
                    println!("exit {:?} -> page {}", code, if done { "Done" } else { "Error" });
                    return if done && code == Some(0) { 0 } else { 1 };
                }
                _ => {}
            }
        }
        1
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--self-test-list") => std::process::exit(self_test_list()),
        Some("--self-test-install") => {
            let dev = args.get(1).map(String::as_str).unwrap_or("/dev/fake");
            std::process::exit(self_test_install(dev));
        }
        _ => {}
    }
    let settings = Settings::default().size_limits(Limits::NONE.min_width(560.0).min_height(420.0));
    cosmic::app::run::<App>(settings, ())?;
    Ok(())
}
