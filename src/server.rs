//! The background process that owns the sessions. It outlives the clients,
//! so sessions survive the program being exited or the terminal closing.
//!
//! Every client gets its own picker and attachment; the menu logic runs
//! here and clients only relay keys and terminal output.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread;

use signal_hook::consts::{SIGCHLD, SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::menu::{Action, BUILTINS, Item, Menu, Outcome, Target};
use crate::protocol::{self, ClientMsg, ServerMsg, VERSION};
use crate::session::{Session, SessionId};
use crate::{paths, term};

pub enum Event {
    Output(SessionId, Vec<u8>),
    Exited(SessionId),
    ChildExited,
    Connected {
        stream: UnixStream,
        version: u32,
        size: (u16, u16),
        cwd: PathBuf,
    },
    Message(ClientId, ClientMsg),
    Disconnected(ClientId),
    Quit,
}

type ClientId = u64;

const PREFIX: u8 = 0x02; // ctrl-b

enum Mode {
    Attached(SessionId),
    Menu(Menu),
}

struct Client {
    mode: Mode,
    size: (u16, u16),
    /// Starting directory for sessions created without a previous session.
    cwd: PathBuf,
    out: Sender<ServerMsg>,
}

struct Server {
    sessions: BTreeMap<SessionId, Session>,
    /// Most recently used first.
    mru: Vec<SessionId>,
    next_session: SessionId,
    clients: BTreeMap<ClientId, Client>,
    next_client: ClientId,
    events: Sender<Event>,
    quit: bool,
}

impl Server {
    fn send(&self, cid: ClientId, bytes: impl Into<Vec<u8>>) {
        if let Some(client) = self.clients.get(&cid) {
            let _ = client.out.send(ServerMsg::Output(bytes.into()));
        }
    }

    fn connect(&mut self, stream: UnixStream, version: u32, size: (u16, u16), cwd: PathBuf) {
        let cid = self.next_client;
        self.next_client += 1;
        let Ok(out) = spawn_client_threads(cid, stream, self.events.clone()) else {
            return;
        };
        if version != VERSION {
            let msg = "the running server is from a different version; end its sessions and kill it first\r\n";
            let _ = out.send(ServerMsg::Output(msg.into()));
            let _ = out.send(ServerMsg::Exit);
            return;
        }
        let client = Client {
            mode: Mode::Attached(0),
            size,
            cwd,
            out,
        };
        self.clients.insert(cid, client);
        self.open_menu(cid, None);
    }

    fn message(&mut self, cid: ClientId, msg: ClientMsg) {
        match msg {
            ClientMsg::Hello { .. } => {}
            ClientMsg::Input(bytes) => self.handle_input(cid, &bytes),
            ClientMsg::Resize(size) => self.resize_client(cid, size),
        }
    }

    fn handle_input(&mut self, cid: ClientId, mut input: &[u8]) {
        while !input.is_empty() {
            let Some(client) = self.clients.get_mut(&cid) else {
                return;
            };
            match &mut client.mode {
                Mode::Attached(id) => {
                    let id = *id;
                    let session = self.sessions.get_mut(&id).unwrap();
                    // With several clients on one session, whoever types
                    // decides its size.
                    session.resize(client.size);
                    let split = input.iter().position(|&b| b == PREFIX);
                    let (keys, rest) = input.split_at(split.unwrap_or(input.len()));
                    session.write(keys);
                    if split.is_none() {
                        return;
                    }
                    input = &rest[1..];
                    self.open_menu(cid, Some(id));
                }
                Mode::Menu(menu) => {
                    let (outcome, consumed) = menu.handle_input(input);
                    input = &input[consumed..];
                    self.apply(cid, outcome);
                }
            }
        }
    }

    fn menu(&mut self, cid: ClientId) -> Option<&mut Menu> {
        match &mut self.clients.get_mut(&cid)?.mode {
            Mode::Menu(menu) => Some(menu),
            Mode::Attached(_) => None,
        }
    }

    fn apply(&mut self, cid: ClientId, outcome: Outcome) {
        let Some(menu) = self.menu(cid) else { return };
        let return_to = menu.return_to;
        match outcome {
            Outcome::Continue => self.render_menu(cid),
            Outcome::Cancel => self.leave_menu(cid),
            Outcome::PassPrefix => match return_to {
                Some(id) => {
                    self.attach(cid, id);
                    self.sessions.get_mut(&id).unwrap().write(&[PREFIX]);
                }
                None => self.leave_menu(cid),
            },
            Outcome::Select(Target::Session(id)) => self.attach(cid, id),
            Outcome::Select(Target::Action(Action::New)) => self.prompt_name(cid, String::new()),
            Outcome::Select(Target::Action(Action::Exit)) => self.detach(cid),
            Outcome::Select(Target::Action(Action::KillServer)) => self.quit = true,
            Outcome::NameSession(name) => self.prompt_name(cid, name),
            Outcome::Create(name) => {
                if self.name_taken(&name) {
                    self.menu_message(cid, format!("session '{name}' already exists"));
                } else {
                    self.create(cid, name);
                }
            }
        }
    }

    fn prompt_name(&mut self, cid: ClientId, name: String) {
        let default = (0..)
            .map(|n: u64| n.to_string())
            .find(|n| !self.name_taken(n))
            .unwrap();
        if let Some(menu) = self.menu(cid) {
            menu.prompt_name(name, default);
        }
        self.render_menu(cid);
    }

    fn name_taken(&self, name: &str) -> bool {
        self.sessions.values().any(|s| s.name == name)
    }

    /// Escape: back to the previous session. Without any session there is
    /// nothing to go back to, so the picker stays open.
    fn leave_menu(&mut self, cid: ClientId) {
        let Some(menu) = self.menu(cid) else { return };
        let return_to = menu.return_to;
        if let Some(id) = return_to.or_else(|| self.mru.first().copied()) {
            self.attach(cid, id);
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
            let session = &self.sessions[&id];
            let name = &session.name;
            let builtin = BUILTINS.iter().any(|(b, ..)| b == name);
            let current = (Some(id) == return_to).then(|| "(current)".to_string());
            Item {
                label: if builtin {
                    format!("s#{name}")
                } else {
                    name.clone()
                },
                target: Target::Session(id),
                note: session.attention.clone().or(current),
                alert: session.attention.is_some(),
            }
        });
        let actions = BUILTINS.iter().map(|(name, action, note)| Item {
            label: name.to_string(),
            target: Target::Action(*action),
            note: Some(note.to_string()),
            alert: false,
        });
        sessions.chain(actions).collect()
    }

    fn open_menu(&mut self, cid: ClientId, return_to: Option<SessionId>) {
        let items = self.menu_items(return_to);
        let Some(client) = self.clients.get_mut(&cid) else {
            return;
        };
        client.mode = Mode::Menu(Menu::new(return_to, items));
        // The alternate screen preserves the session's display and
        // scrollback underneath the menu.
        self.send(cid, format!("\x1b[?1049h{}", term::MODE_RESET));
        self.render_menu(cid);
    }

    fn render_menu(&self, cid: ClientId) {
        if let Some(Client {
            mode: Mode::Menu(menu),
            size,
            ..
        }) = self.clients.get(&cid)
        {
            self.send(cid, menu.render(*size));
        }
    }

    fn menu_message(&mut self, cid: ClientId, message: String) {
        if let Some(menu) = self.menu(cid) {
            menu.set_message(message);
        }
        self.render_menu(cid);
    }

    /// Re-lists sessions in every open menu after one was added or removed.
    fn refresh_menus(&mut self) {
        let cids: Vec<_> = self.clients.keys().copied().collect();
        for cid in cids {
            let Some(menu) = self.menu(cid) else { continue };
            let return_to = menu.return_to;
            let items = self.menu_items(return_to);
            self.menu(cid).unwrap().set_items(items);
            self.render_menu(cid);
        }
    }

    fn attach(&mut self, cid: ClientId, id: SessionId) {
        let Some(client) = self.clients.get_mut(&cid) else {
            return;
        };
        let previous = std::mem::replace(&mut client.mode, Mode::Attached(id));
        let session = self.sessions.get_mut(&id).unwrap();
        session.attention = None;
        let resized = session.resize(client.size);
        let replay = match previous {
            Mode::Menu(mut menu)
                if menu.return_to == Some(id) && !resized && !session.alternate_screen() =>
            {
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
        self.send(cid, out);
        self.mru.retain(|&s| s != id);
        self.mru.insert(0, id);
    }

    fn create(&mut self, cid: ClientId, name: String) {
        let Some(client) = self.clients.get(&cid) else {
            return;
        };
        let return_to = match &client.mode {
            Mode::Menu(menu) => menu.return_to,
            Mode::Attached(id) => Some(*id),
        };
        let cwd = return_to
            .and_then(|id| self.sessions[&id].cwd())
            .unwrap_or_else(|| client.cwd.clone());
        let id = self.next_session;
        match Session::spawn(id, name, client.size, Some(cwd), self.events.clone()) {
            Ok(session) => {
                self.next_session += 1;
                self.sessions.insert(id, session);
                self.attach(cid, id);
                self.refresh_menus();
            }
            Err(e) => self.menu_message(cid, format!("failed to start shell: {e}")),
        }
    }

    fn output(&mut self, id: SessionId, bytes: &[u8]) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let alerts = session.process(bytes);
        let viewed = self
            .clients
            .values()
            .any(|c| matches!(c.mode, Mode::Attached(a) if a == id));
        if !viewed && (alerts.bell || !alerts.notifications.is_empty()) {
            // Flag the session in the picker, and pass the request on so the
            // desktop can still highlight the terminal.
            let mut forward = Vec::new();
            if alerts.bell {
                session.attention = Some("bell".into());
                forward.push(0x07);
            }
            for notification in alerts.notifications {
                session.attention = Some(notification.text);
                forward.extend(notification.sequence);
            }
            for client in self.clients.values() {
                let _ = client.out.send(ServerMsg::Output(forward.clone()));
            }
            self.refresh_menus();
        }
        for client in self.clients.values_mut() {
            match &mut client.mode {
                Mode::Attached(active) if *active == id => {
                    let _ = client.out.send(ServerMsg::Output(bytes.to_vec()));
                }
                Mode::Menu(menu) if menu.return_to == Some(id) => menu.buffer_output(bytes),
                _ => {}
            }
        }
    }

    fn remove(&mut self, id: SessionId) {
        if self.sessions.remove(&id).is_none() {
            return;
        }
        self.mru.retain(|&s| s != id);
        let cids: Vec<_> = self.clients.keys().copied().collect();
        for cid in cids {
            match &mut self.clients.get_mut(&cid).unwrap().mode {
                Mode::Attached(active) if *active == id => self.open_menu(cid, None),
                Mode::Attached(_) => {}
                Mode::Menu(menu) => {
                    if menu.return_to == Some(id) {
                        menu.forget_return_target();
                    }
                }
            }
        }
        self.refresh_menus();
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

    fn resize_client(&mut self, cid: ClientId, size: (u16, u16)) {
        let Some(client) = self.clients.get_mut(&cid) else {
            return;
        };
        client.size = size;
        match &client.mode {
            Mode::Attached(id) => {
                self.sessions.get_mut(id).unwrap().resize(size);
            }
            Mode::Menu(_) => self.render_menu(cid),
        }
    }

    /// Ends the client, leaving all sessions running.
    fn detach(&mut self, cid: ClientId) {
        let Some(client) = self.clients.remove(&cid) else {
            return;
        };
        let on_alternate_screen = match client.mode {
            Mode::Menu(_) => true,
            Mode::Attached(id) => self.sessions.get(&id).is_some_and(|s| s.alternate_screen()),
        };
        // ?1049l also restores the cursor saved on entering the alternate
        // screen, so only send it when there is one to leave. Otherwise move
        // below the session's output for the shell the user returns to.
        let leave = if on_alternate_screen {
            "\x1b[?1049l"
        } else {
            "\r\n"
        };
        let reset = format!("{leave}{}", term::reset());
        let _ = client.out.send(ServerMsg::Output(reset.into()));
        let _ = client.out.send(ServerMsg::Exit);
    }

    fn done(&self) -> bool {
        self.quit || (self.clients.is_empty() && self.sessions.is_empty())
    }
}

/// Starts the threads moving messages between a client socket and the
/// event loop. Returns the sender for messages to the client.
fn spawn_client_threads(
    cid: ClientId,
    stream: UnixStream,
    events: Sender<Event>,
) -> io::Result<Sender<ServerMsg>> {
    let mut reader = stream.try_clone()?;
    thread::spawn(move || {
        while let Ok(msg) = ClientMsg::read(&mut reader) {
            if events.send(Event::Message(cid, msg)).is_err() {
                return;
            }
        }
        let _ = events.send(Event::Disconnected(cid));
    });

    // A separate writer keeps one slow client from stalling everything else.
    let (tx, rx) = mpsc::channel::<ServerMsg>();
    let mut writer = stream;
    thread::spawn(move || {
        for msg in rx {
            if protocol::send(&mut writer, &msg.encode()).is_err() {
                break;
            }
        }
        let _ = writer.shutdown(std::net::Shutdown::Both);
    });
    Ok(tx)
}

fn bind(path: &Path) -> io::Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "a server is already running",
            ));
        }
        // Left over from a server that died.
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path)
}

pub fn run() -> io::Result<()> {
    // Keep panics and errors somewhere they can be found.
    if let Ok(log) = std::fs::File::create(paths::log_path()?) {
        use std::os::fd::AsRawFd;
        // SAFETY: dup2 onto stderr with a valid fd.
        unsafe { libc::dup2(log.as_raw_fd(), 2) };
    }
    let path = paths::socket_path()?;
    let listener = bind(&path)?;

    let (tx, rx) = mpsc::channel();

    let mut signals = Signals::new([SIGCHLD, SIGTERM, SIGINT, SIGHUP])?;
    let signal_tx = tx.clone();
    thread::spawn(move || {
        for signal in signals.forever() {
            let event = match signal {
                SIGCHLD => Event::ChildExited,
                // The server has no terminal; a hangup means nothing to it.
                SIGHUP => continue,
                _ => Event::Quit,
            };
            if signal_tx.send(event).is_err() {
                return;
            }
        }
    });

    let accept_tx = tx.clone();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let tx = accept_tx.clone();
            // Read the greeting off the event loop so a silent client
            // cannot block it.
            thread::spawn(move || {
                if let Ok(ClientMsg::Hello { version, size, cwd }) = ClientMsg::read(&mut stream) {
                    let _ = tx.send(Event::Connected {
                        stream,
                        version,
                        size,
                        cwd,
                    });
                }
            });
        }
    });

    let mut server = Server {
        sessions: BTreeMap::new(),
        mru: Vec::new(),
        next_session: 0,
        clients: BTreeMap::new(),
        next_client: 0,
        events: tx,
        quit: false,
    };

    while let Ok(event) = rx.recv() {
        match event {
            Event::Output(id, bytes) => server.output(id, &bytes),
            Event::Exited(id) => server.remove(id),
            Event::ChildExited => server.reap_children(),
            Event::Connected {
                stream,
                version,
                size,
                cwd,
            } => server.connect(stream, version, size, cwd),
            Event::Message(cid, msg) => server.message(cid, msg),
            Event::Disconnected(cid) => {
                server.clients.remove(&cid);
            }
            Event::Quit => server.quit = true,
        }
        if server.done() {
            break;
        }
    }

    // Tell remaining clients to go and hang up the sessions' shells, as a
    // terminal closing would.
    for session in server.sessions.values() {
        // SAFETY: plain kill; each shell leads its own process group.
        unsafe { libc::kill(-session.pid, libc::SIGHUP) };
    }
    let cids: Vec<_> = server.clients.keys().copied().collect();
    for cid in cids {
        server.detach(cid);
    }
    let _ = std::fs::remove_file(&path);
    // Give the writer threads a moment to deliver the goodbye.
    thread::sleep(std::time::Duration::from_millis(50));
    Ok(())
}
