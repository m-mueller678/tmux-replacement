//! The program running in the user's terminal: forwards keys and size
//! changes to the server and writes what it sends back to the terminal.
//!
//! Connected to a remote server, the client also keeps a copy of every
//! session it showed. When the connection drops it keeps showing them,
//! offers a picker over them, and reconnects in the background.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
use signal_hook::iterator::Signals;

use crate::menu::{Action, Item, Menu, Outcome, Target};
use crate::protocol::{self, ClientMsg, ServerMsg, VERSION};
use crate::session::{self, SCROLLBACK, nz};
use crate::{paths, term};

pub enum Server {
    Local,
    /// Reached through `ssh <destination> <bin> --proxy`.
    Remote {
        destination: String,
        bin: String,
    },
}

enum Event {
    Input(Vec<u8>),
    Resize,
    /// A message on the connection with the given generation.
    Message(u64, ServerMsg),
    /// The connection with the given generation ended.
    Lost(u64, String),
    Retry,
    Quit,
}

const PREFIX: u8 = 0x02; // ctrl-b
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);

/// The sending half of a connection to the server.
struct Connection {
    writer: Box<dyn Write + Send>,
    close: Box<dyn FnMut() + Send>,
}

impl Connection {
    fn send(&mut self, msg: ClientMsg) {
        // A failed write surfaces as a lost connection on the reading side.
        let _ = protocol::send(&mut self.writer, &msg.encode());
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        (self.close)();
    }
}

/// Opens a connection; its messages arrive as events tagged with `generation`.
/// `interactive` lets ssh prompt on the terminal, before raw mode is on.
fn connect(
    server: &Server,
    generation: u64,
    events: Sender<Event>,
    interactive: bool,
) -> io::Result<Connection> {
    match server {
        Server::Local => {
            let stream = connect_or_start(&paths::socket_path()?)?;
            let mut reader = stream.try_clone()?;
            thread::spawn(move || {
                read_messages(&mut reader, generation, &events, || "server closed".into())
            });
            let closer = stream.try_clone()?;
            Ok(Connection {
                writer: Box::new(stream),
                close: Box::new(move || {
                    let _ = closer.shutdown(std::net::Shutdown::Both);
                }),
            })
        }
        Server::Remote { destination, bin } => {
            let mut cmd = Command::new("ssh");
            // Notice a dead network within ~15s instead of the TCP timeout.
            cmd.args([
                "-T",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=3",
            ]);
            if !interactive {
                // Nobody can answer a prompt in the middle of the session.
                cmd.args(["-o", "BatchMode=yes"]);
            }
            cmd.args(["--", destination, bin, "--proxy"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(if interactive {
                    Stdio::inherit()
                } else {
                    Stdio::piped()
                });
            let mut child = cmd
                .spawn()
                .map_err(|e| io::Error::new(e.kind(), format!("ssh: {e}")))?;
            let stdin: ChildStdin = child.stdin.take().unwrap();
            let mut stdout = child.stdout.take().unwrap();

            // Keep ssh's last complaint to explain why reconnecting fails.
            let last_error = Arc::new(Mutex::new(String::new()));
            if let Some(stderr) = child.stderr.take() {
                let last_error = last_error.clone();
                thread::spawn(move || {
                    for line in io::BufRead::lines(io::BufReader::new(stderr)).map_while(Result::ok)
                    {
                        if !line.trim().is_empty() {
                            *last_error.lock().unwrap() = line.trim().to_string();
                        }
                    }
                });
            }

            let child = Arc::new(Mutex::new(child));
            let reaper = child.clone();
            thread::spawn(move || {
                read_messages(&mut stdout, generation, &events, || {
                    let _ = reaper.lock().unwrap().wait();
                    // Let the stderr reader catch up with ssh's last words.
                    thread::sleep(Duration::from_millis(50));
                    let error = last_error.lock().unwrap().clone();
                    if error.is_empty() {
                        "connection lost".into()
                    } else {
                        error
                    }
                })
            });
            Ok(Connection {
                writer: Box::new(stdin),
                close: Box::new(move || {
                    let _ = child.lock().unwrap().kill();
                }),
            })
        }
    }
}

fn read_messages(
    r: &mut impl Read,
    generation: u64,
    events: &Sender<Event>,
    reason: impl FnOnce() -> String,
) {
    while let Ok(msg) = ServerMsg::read(r) {
        if events.send(Event::Message(generation, msg)).is_err() {
            return;
        }
    }
    let _ = events.send(Event::Lost(generation, reason()));
}

/// A session as last shown, kept to show it while disconnected.
struct Cached {
    name: String,
    parser: vt100::Parser,
}

struct Client {
    server: Server,
    events: Sender<Event>,
    token: u64,
    /// Generation of the current connection; events from older ones are
    /// stale.
    generation: u64,
    conn: Option<Connection>,
    /// Whether the current connection delivered anything yet.
    established: bool,
    size: (u16, u16),
    /// The session on screen, live or cached; None while in a picker.
    view: Option<String>,
    /// Most recently shown first. Only kept for remote servers.
    cache: Vec<Cached>,
    offline: Option<Offline>,
    quit: bool,
    detached: bool,
}

struct Offline {
    reason: String,
    retry_delay: Duration,
    menu: Option<Menu>,
}

impl Client {
    fn remote(&self) -> bool {
        matches!(self.server, Server::Remote { .. })
    }

    fn hello(&self) -> ClientMsg {
        ClientMsg::Hello {
            version: VERSION,
            size: self.size,
            cwd: std::env::current_dir().unwrap_or_default(),
            token: self.token,
            resume: self.view.clone(),
        }
    }

    fn message(&mut self, msg: ServerMsg) {
        if !self.established {
            self.established = true;
            if self.offline.take().is_some() {
                self.view = None;
            }
        }
        match msg {
            ServerMsg::Output(bytes) => term::write(&bytes),
            ServerMsg::SessionOutput(bytes) => {
                term::write(&bytes);
                if let Some(cached) = self
                    .cache
                    .first_mut()
                    .filter(|c| Some(&c.name) == self.view.as_ref())
                {
                    cached.parser.process(&bytes);
                }
            }
            ServerMsg::View { session, fresh } => {
                if let (true, Some(name)) = (self.remote(), &session) {
                    let existing = self.cache.iter().position(|c| &c.name == name);
                    let cached = match existing {
                        Some(i) if !fresh => self.cache.remove(i),
                        _ => {
                            if let Some(i) = existing {
                                self.cache.remove(i);
                            }
                            let (rows, cols) = self.size;
                            Cached {
                                name: name.clone(),
                                parser: vt100::Parser::new(nz(rows), nz(cols), SCROLLBACK),
                            }
                        }
                    };
                    self.cache.insert(0, cached);
                }
                self.view = session;
            }
            ServerMsg::Sessions(names) => self.cache.retain(|c| names.contains(&c.name)),
            ServerMsg::Exit => {
                self.detached = true;
                self.quit = true;
            }
        }
    }

    fn lost(&mut self, reason: String) {
        self.conn = None;
        if !self.remote() {
            self.quit = true;
            return;
        }
        if !self.established && self.offline.is_none() {
            // Never got through in the first place.
            eprintln!("{reason}");
            self.quit = true;
            return;
        }
        let retry_delay = match &mut self.offline {
            Some(offline) => {
                offline.reason = reason;
                offline.retry_delay = (offline.retry_delay * 2).min(MAX_RETRY_DELAY);
                offline.retry_delay
            }
            None => {
                let delay = Duration::from_secs(1);
                self.offline = Some(Offline {
                    reason,
                    retry_delay: delay,
                    menu: None,
                });
                // Lost in the server's picker: continue in the offline one.
                if self.view.is_none() {
                    self.open_offline_menu();
                }
                delay
            }
        };
        self.draw_offline();
        let events = self.events.clone();
        thread::spawn(move || {
            thread::sleep(retry_delay);
            let _ = events.send(Event::Retry);
        });
    }

    fn retry(&mut self) {
        self.generation += 1;
        self.established = false;
        match connect(&self.server, self.generation, self.events.clone(), false) {
            Ok(mut conn) => {
                conn.send(self.hello());
                self.conn = Some(conn);
            }
            Err(e) => {
                let _ = self
                    .events
                    .send(Event::Lost(self.generation, e.to_string()));
            }
        }
    }

    fn status(&self) -> String {
        let Server::Remote { destination, .. } = &self.server else {
            return String::new();
        };
        let reason = self.offline.as_ref().map_or("", |o| o.reason.as_str());
        format!("disconnected from {destination}, reconnecting ({reason})")
    }

    /// Shows the disconnect on top of whatever is on screen.
    fn draw_offline(&mut self) {
        let status = self.status();
        let size = self.size;
        let Some(offline) = &mut self.offline else {
            return;
        };
        match &mut offline.menu {
            Some(menu) => {
                menu.set_message(status);
                term::write(menu.render(size).as_bytes());
            }
            None => {
                let (rows, cols) = size;
                let text = format!(" {status} · ctrl-b: sessions seen so far ");
                let text: String = text.chars().take(cols as usize).collect();
                // Overwrites the bottom row until the reconnect repaints it.
                let line = format!("\x1b7\x1b[{rows};1H\x1b[0;7m{text}\x1b[K\x1b[0m\x1b8");
                term::write(line.as_bytes());
            }
        }
    }

    fn open_offline_menu(&mut self) {
        let mut items: Vec<Item> = self
            .cache
            .iter()
            .enumerate()
            .map(|(i, c)| Item {
                label: c.name.clone(),
                target: Target::Session(i as u64),
                note: Some(
                    if Some(&c.name) == self.view.as_ref() {
                        "(current, cached)"
                    } else {
                        "(cached)"
                    }
                    .into(),
                ),
                alert: false,
            })
            .collect();
        // Most recent but one first, as in the server's picker.
        if items.len() > 1 && self.view.as_ref() == Some(&self.cache[0].name) {
            items.swap(0, 1);
        }
        items.push(Item {
            label: "exit".into(),
            target: Target::Action(Action::Exit),
            note: Some("quit; remote sessions keep running".into()),
            alert: false,
        });
        term::write(format!("\x1b[?1049h{}", term::MODE_RESET).as_bytes());
        if let Some(offline) = &mut self.offline {
            offline.menu = Some(Menu::new(None, items));
        }
        self.draw_offline();
    }

    /// Repaints a cached session from the local copy.
    fn show_cached(&mut self, i: usize) {
        let mut cached = self.cache.remove(i);
        term::write(session::repaint(&mut cached.parser).as_bytes());
        self.view = Some(cached.name.clone());
        self.cache.insert(0, cached);
        if let Some(offline) = &mut self.offline {
            offline.menu = None;
        }
        self.draw_offline();
    }

    fn input(&mut self, mut input: &[u8]) {
        if let Some(conn) = &mut self.conn
            && self.offline.is_none()
        {
            conn.send(ClientMsg::Input(input.to_vec()));
            return;
        }
        // Offline: only the picker over cached sessions takes keys.
        while !input.is_empty() && !self.quit {
            let Some(offline) = &mut self.offline else {
                return;
            };
            let Some(menu) = &mut offline.menu else {
                match input.iter().position(|&b| b == PREFIX) {
                    Some(p) => {
                        input = &input[p + 1..];
                        self.open_offline_menu();
                    }
                    None => return,
                }
                continue;
            };
            let (outcome, consumed) = menu.handle_input(input);
            input = &input[consumed..];
            match outcome {
                Outcome::Continue => self.draw_offline(),
                Outcome::Cancel | Outcome::PassPrefix => {
                    if let Some(i) = self
                        .cache
                        .iter()
                        .position(|c| Some(&c.name) == self.view.as_ref())
                    {
                        self.show_cached(i);
                    }
                }
                Outcome::Select(Target::Session(i)) => self.show_cached(i as usize),
                Outcome::Select(Target::Action(Action::Exit)) => self.quit = true,
                _ => {
                    menu.set_message("not available while disconnected".into());
                    let size = self.size;
                    term::write(menu.render(size).as_bytes());
                }
            }
        }
    }

    fn resize(&mut self) {
        self.size = term::size();
        let (rows, cols) = self.size;
        for cached in &mut self.cache {
            cached.parser.set_size(nz(rows), nz(cols));
        }
        if let (Some(conn), None) = (&mut self.conn, &self.offline) {
            conn.send(ClientMsg::Resize(self.size));
            return;
        }
        let showing_cached = self.offline.as_ref().is_some_and(|o| o.menu.is_none());
        match self
            .cache
            .iter()
            .position(|c| Some(&c.name) == self.view.as_ref())
        {
            Some(i) if showing_cached => self.show_cached(i),
            _ => self.draw_offline(),
        }
    }
}

pub fn run(server: Server) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut client = Client {
        server,
        events: tx.clone(),
        token: random_token(),
        generation: 0,
        conn: None,
        established: false,
        size: term::size(),
        view: None,
        cache: Vec::new(),
        offline: None,
        quit: false,
        detached: false,
    };
    let mut conn = connect(&client.server, 0, tx.clone(), true)?;
    conn.send(client.hello());
    client.conn = Some(conn);

    // Wait until the server answers before taking over the terminal, so
    // ssh can still ask for a password.
    let first = match rx.recv() {
        Ok(Event::Message(_, msg)) => msg,
        Ok(Event::Lost(_, reason)) => return Err(io::Error::other(reason)),
        _ => return Err(io::Error::other("connection failed")),
    };
    term::enable_raw_mode().map_err(io::Error::from)?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        fallback_cleanup();
        term::restore_mode();
        default_hook(info);
    }));

    let mut signals = Signals::new([SIGWINCH, SIGTERM, SIGHUP, SIGINT])?;
    let signal_tx = tx.clone();
    thread::spawn(move || {
        for signal in signals.forever() {
            let event = if signal == SIGWINCH {
                Event::Resize
            } else {
                Event::Quit
            };
            if signal_tx.send(event).is_err() {
                return;
            }
        }
    });

    let input_tx = tx;
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || input_tx.send(Event::Input(buf[..n].to_vec())).is_err() {
                break;
            }
        }
        let _ = input_tx.send(Event::Quit);
    });

    client.message(first);
    while !client.quit {
        let Ok(event) = rx.recv() else { break };
        match event {
            Event::Input(bytes) => client.input(&bytes),
            Event::Resize => client.resize(),
            Event::Message(generation, msg) if generation == client.generation => {
                client.message(msg)
            }
            Event::Lost(generation, reason) if generation == client.generation => {
                client.lost(reason)
            }
            Event::Message(..) | Event::Lost(..) => {}
            Event::Retry => client.retry(),
            Event::Quit => client.quit = true,
        }
    }
    if !client.detached {
        // An offline picker is on the alternate screen; leave it properly.
        if client.offline.as_ref().is_some_and(|o| o.menu.is_some()) {
            term::write(b"\x1b[?1049l");
        }
        fallback_cleanup();
    }
    term::restore_mode();
    Ok(())
}

/// Bytes the remote proxy relays between ssh and the local server socket.
pub fn proxy() -> io::Result<()> {
    let stream = connect_or_start(&paths::socket_path()?)?;
    let mut to_server = stream.try_clone()?;
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut to_server);
        // ssh went away; so does this client.
        std::process::exit(0);
    });
    let mut from_server = stream;
    let mut stdout = io::stdout().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = from_server.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        stdout.write_all(&buf[..n])?;
        stdout.flush()?;
    }
}

fn random_token() -> u64 {
    let mut bytes = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut bytes);
    }
    u64::from_le_bytes(bytes) ^ u64::from(std::process::id())
}

/// Used when the server did not get to clean up the terminal. Unlike
/// ?1049l, ?1047l leaves the cursor alone when not on the alternate screen.
fn fallback_cleanup() {
    term::write(format!("\x1b[?1047l{}\r\n", term::reset()).as_bytes());
}

fn connect_or_start(path: &Path) -> io::Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(path) {
        return Ok(stream);
    }
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("--server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe. A new session detaches the
    // server from this terminal so it survives the terminal closing.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(e) if Instant::now() > deadline => {
                let log = paths::log_path()?;
                return Err(io::Error::new(
                    e.kind(),
                    format!("could not reach server: {e} (see {})", log.display()),
                ));
            }
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}
