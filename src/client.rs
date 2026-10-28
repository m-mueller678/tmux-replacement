//! The program running in the user's terminal: forwards keys and size
//! changes to the server and writes what it sends back to the terminal.

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
use signal_hook::iterator::Signals;

use crate::protocol::{self, ClientMsg, ServerMsg, VERSION};
use crate::{paths, term};

pub fn run() -> io::Result<()> {
    let path = paths::socket_path()?;
    let stream = connect_or_start(&path)?;

    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let send = {
        let writer = writer.clone();
        move |msg: ClientMsg| protocol::send(&mut *writer.lock().unwrap(), &msg.encode())
    };
    send(ClientMsg::Hello {
        version: VERSION,
        size: term::size(),
        cwd: std::env::current_dir().unwrap_or_default(),
    })?;

    term::enable_raw_mode().map_err(io::Error::from)?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        fallback_cleanup();
        term::restore_mode();
        default_hook(info);
    }));

    let mut signals = Signals::new([SIGWINCH, SIGTERM, SIGHUP, SIGINT])?;
    let signal_send = send.clone();
    let signal_stream = stream.try_clone()?;
    thread::spawn(move || {
        for signal in signals.forever() {
            if signal == SIGWINCH {
                let _ = signal_send(ClientMsg::Resize(term::size()));
            } else {
                // Ends the read loop below; the server keeps the sessions.
                let _ = signal_stream.shutdown(std::net::Shutdown::Both);
            }
        }
    });

    let input_stream = stream.try_clone()?;
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || send(ClientMsg::Input(buf[..n].to_vec())).is_err() {
                break;
            }
        }
        let _ = input_stream.shutdown(std::net::Shutdown::Both);
    });

    let mut reader = stream;
    let detached = loop {
        match ServerMsg::read(&mut reader) {
            Ok(ServerMsg::Output(bytes)) => term::write(&bytes),
            Ok(ServerMsg::Exit) => break true,
            Err(_) => break false,
        }
    };
    if !detached {
        fallback_cleanup();
    }
    term::restore_mode();
    Ok(())
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
