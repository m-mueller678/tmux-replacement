mod client;
mod menu;
mod paths;
mod protocol;
mod server;
mod session;
mod term;

fn main() {
    let result = if std::env::args().nth(1).as_deref() == Some("--server") {
        server::run()
    } else {
        if std::env::var_os("TMUX_REPLACEMENT_SESSION").is_some() {
            eprintln!("already running inside a session");
            std::process::exit(1);
        }
        // SAFETY: isatty only inspects the fds.
        if unsafe { libc::isatty(0) == 0 || libc::isatty(1) == 0 } {
            eprintln!("stdin and stdout must be a terminal");
            std::process::exit(1);
        }
        client::run()
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
