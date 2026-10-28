mod menu;
mod session;
mod term;

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::mpsc::{self, Sender};
use std::thread;

use signal_hook::consts::{SIGCHLD, SIGHUP, SIGTERM, SIGWINCH};
use signal_hook::iterator::Signals;

use menu::{Action, BUILTINS, Item, Menu, Outcome, Target};
use session::{Session, SessionId};

pub enum Event {
    Input(Vec<u8>),
    Output(SessionId, Vec<u8>),
    Exited(SessionId),
    ChildExited,
    Resize,
    Quit,
}

const PREFIX: u8 = 0x02; // ctrl-b

enum Mode {
    Attached(SessionId),
    Menu(Menu),
}

struct App {
    sessions: BTreeMap<SessionId, Session>,
    /// Most recently used first.
    mru: Vec<SessionId>,
    next_id: SessionId,
    mode: Mode,
    size: (u16, u16),
    events: Sender<Event>,
    quit: bool,
}

impl App {
    fn handle_input(&mut self, mut input: &[u8]) {
        while !input.is_empty() && !self.quit {
            match &mut self.mode {
                Mode::Attached(id) => {
                    let id = *id;
                    let split = input.iter().position(|&b| b == PREFIX);
                    let (keys, rest) = input.split_at(split.unwrap_or(input.len()));
                    self.sessions.get_mut(&id).unwrap().write(keys);
                    if split.is_none() {
                        return;
                    }
                    input = &rest[1..];
                    self.open_menu(Some(id));
                }
                Mode::Menu(menu) => {
                    let (outcome, consumed) = menu.handle_input(input);
                    input = &input[consumed..];
                    self.apply(outcome);
                }
            }
        }
    }

    fn apply(&mut self, outcome: Outcome) {
        let Mode::Menu(menu) = &self.mode else { return };
        let return_to = menu.return_to;
        match outcome {
            Outcome::Continue => self.render_menu(),
            Outcome::Cancel => self.leave_menu(),
            Outcome::PassPrefix => match return_to {
                Some(id) => {
                    self.attach(id);
                    self.sessions.get_mut(&id).unwrap().write(&[PREFIX]);
                }
                None => self.leave_menu(),
            },
            Outcome::Select(Target::Session(id)) => self.attach(id),
            Outcome::Select(Target::Action(Action::New)) => {
                let name = (0..)
                    .map(|n: u64| n.to_string())
                    .find(|n| !self.name_taken(n))
                    .unwrap();
                self.create(name);
            }
            Outcome::Create(name) => {
                if self.name_taken(&name) {
                    self.menu_message(format!("session '{name}' already exists"));
                } else {
                    self.create(name);
                }
            }
        }
    }

    fn name_taken(&self, name: &str) -> bool {
        self.sessions.values().any(|s| s.name == name)
    }

    /// Escape: back to the previous session, or quit if there is none.
    fn leave_menu(&mut self) {
        let Mode::Menu(menu) = &self.mode else { return };
        match menu.return_to.or_else(|| self.mru.first().copied()) {
            Some(id) => self.attach(id),
            None => self.quit = true,
        }
    }

    fn menu_items(&self, return_to: Option<SessionId>) -> Vec<Item> {
        let mut order = self.mru.clone();
        // Put the previously active session second, so the top entry is the
        // last session used before it (alt-tab style).
        if order.len() > 1 && return_to == Some(order[0]) {
            order.swap(0, 1);
        }
        let sessions = order.into_iter().map(|id| {
            let name = &self.sessions[&id].name;
            let builtin = BUILTINS.iter().any(|(b, ..)| b == name);
            Item {
                label: if builtin {
                    format!("s#{name}")
                } else {
                    name.clone()
                },
                target: Target::Session(id),
                note: (Some(id) == return_to).then(|| "(current)".into()),
            }
        });
        let actions = BUILTINS.iter().map(|(name, action, note)| Item {
            label: name.to_string(),
            target: Target::Action(*action),
            note: Some(note.to_string()),
        });
        sessions.chain(actions).collect()
    }

    fn open_menu(&mut self, return_to: Option<SessionId>) {
        // The alternate screen preserves the session's display and
        // scrollback underneath the menu.
        term::write(format!("\x1b[?1049h{}", term::MODE_RESET).as_bytes());
        self.mode = Mode::Menu(Menu::new(return_to, self.menu_items(return_to)));
        self.render_menu();
    }

    fn render_menu(&self) {
        if let Mode::Menu(menu) = &self.mode {
            term::write(menu.render(self.size).as_bytes());
        }
    }

    fn menu_message(&mut self, message: String) {
        if let Mode::Menu(menu) = &mut self.mode {
            menu.set_message(message);
        }
        self.render_menu();
    }

    fn attach(&mut self, id: SessionId) {
        let previous = std::mem::replace(&mut self.mode, Mode::Attached(id));
        let session = self.sessions.get_mut(&id).unwrap();
        let replay = match previous {
            Mode::Menu(mut menu) if menu.return_to == Some(id) && !session.alternate_screen() => {
                menu.take_pending()
            }
            Mode::Attached(current) if current == id => return,
            _ => None,
        };
        let out = match replay {
            // Leaving the alternate screen restores exactly what the outer
            // terminal showed before the menu opened; then catch up.
            Some(pending) => {
                let mut out = b"\x1b[?1049l".to_vec();
                out.extend_from_slice(&pending);
                out.extend_from_slice(session.modes().as_bytes());
                out
            }
            None => session.repaint().into_bytes(),
        };
        term::write(&out);
        self.mru.retain(|&s| s != id);
        self.mru.insert(0, id);
    }

    fn create(&mut self, name: String) {
        let return_to = match &self.mode {
            Mode::Menu(menu) => menu.return_to,
            Mode::Attached(id) => Some(*id),
        };
        let cwd = return_to.and_then(|id| self.sessions[&id].cwd());
        let id = self.next_id;
        match Session::spawn(id, name, self.size, cwd, self.events.clone()) {
            Ok(session) => {
                self.next_id += 1;
                self.sessions.insert(id, session);
                self.attach(id);
            }
            Err(e) => self.menu_message(format!("failed to start shell: {e}")),
        }
    }

    fn output(&mut self, id: SessionId, bytes: &[u8]) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        session.process(bytes);
        match &mut self.mode {
            Mode::Attached(active) if *active == id => term::write(bytes),
            Mode::Menu(menu) if menu.return_to == Some(id) => menu.buffer_output(bytes),
            _ => {}
        }
    }

    fn remove(&mut self, id: SessionId) {
        if self.sessions.remove(&id).is_none() {
            return;
        }
        self.mru.retain(|&s| s != id);
        match &mut self.mode {
            Mode::Attached(active) if *active == id => {
                if self.sessions.is_empty() {
                    self.quit = true;
                } else {
                    self.open_menu(None);
                }
            }
            Mode::Attached(_) => {}
            Mode::Menu(menu) => {
                if menu.return_to == Some(id) {
                    menu.forget_return_target();
                }
                let return_to = menu.return_to;
                let items = self.menu_items(return_to);
                if let Mode::Menu(menu) = &mut self.mode {
                    menu.set_items(items);
                }
                self.render_menu();
            }
        }
    }

    fn reap_children(&mut self) {
        loop {
            let mut status = 0;
            // SAFETY: plain waitpid on any child, non-blocking.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                return;
            }
            let ids: Vec<_> = self
                .sessions
                .iter()
                .filter(|(_, s)| s.pid == pid)
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                self.remove(id);
            }
        }
    }

    fn resize(&mut self) {
        self.size = term::size();
        for session in self.sessions.values_mut() {
            session.resize(self.size);
        }
        self.render_menu();
    }
}

fn main() {
    if std::env::var_os("TMUX_REPLACEMENT_SESSION").is_some() {
        eprintln!("already running inside a session");
        std::process::exit(1);
    }
    // SAFETY: isatty only inspects the fds.
    if unsafe { libc::isatty(0) == 0 || libc::isatty(1) == 0 } {
        eprintln!("stdin and stdout must be a terminal");
        std::process::exit(1);
    }

    let (tx, rx) = mpsc::channel();

    let mut signals = Signals::new([SIGWINCH, SIGCHLD, SIGTERM, SIGHUP])
        .expect("failed to register signal handlers");
    let signal_tx = tx.clone();
    thread::spawn(move || {
        for signal in signals.forever() {
            let event = match signal {
                SIGWINCH => Event::Resize,
                SIGCHLD => Event::ChildExited,
                _ => Event::Quit,
            };
            if signal_tx.send(event).is_err() {
                return;
            }
        }
    });

    let input_tx = tx.clone();
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || input_tx.send(Event::Input(buf[..n].to_vec())).is_err() {
                break;
            }
        }
    });

    if let Err(e) = term::enable_raw_mode() {
        eprintln!("failed to enable raw mode: {e}");
        std::process::exit(1);
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        term::write(format!("\x1b[?1049l{}\x1b[?25h", term::MODE_RESET).as_bytes());
        term::restore_mode();
        default_hook(info);
    }));

    let mut app = App {
        sessions: BTreeMap::new(),
        mru: Vec::new(),
        next_id: 0,
        mode: Mode::Attached(0),
        size: term::size(),
        events: tx,
        quit: false,
    };
    app.open_menu(None);

    while let Ok(event) = rx.recv() {
        match event {
            Event::Input(bytes) => app.handle_input(&bytes),
            Event::Output(id, bytes) => app.output(id, &bytes),
            Event::Exited(id) => app.remove(id),
            Event::ChildExited => app.reap_children(),
            Event::Resize => app.resize(),
            Event::Quit => app.quit = true,
        }
        if app.quit {
            break;
        }
    }

    let leave_alt = if matches!(app.mode, Mode::Menu(_)) {
        "\x1b[?1049l"
    } else {
        ""
    };
    term::write(format!("{leave_alt}{}\x1b[0m\x1b[?25h", term::MODE_RESET).as_bytes());
    term::restore_mode();
}
