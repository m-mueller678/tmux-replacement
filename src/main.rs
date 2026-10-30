mod bar;
mod client;
mod hosts;
mod menu;
mod paths;
mod protocol;
mod server;
mod session;
mod term;

const USAGE: &str = "\
usage: back-burner [--kill-server]

Starts with a picker of machines: this one, or any host from your ssh
config, reached through ssh with back-burner on the remote PATH.
Ctrl-b then opens the session picker. --kill-server ends the server on this
machine and all its sessions, whichever version it is.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["--server"] => server::run(),
        ["--proxy"] => client::proxy(),
        ["--kill-server"] => kill_server(),
        [] => run_client(),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn kill_server() -> std::io::Result<()> {
    if client::kill(&client::Server::Local)? {
        eprintln!("server killed");
    } else {
        eprintln!("no server running");
    }
    Ok(())
}

fn run_client() -> std::io::Result<()> {
    if std::env::var_os("BACK_BURNER_SESSION").is_some() {
        eprintln!("already running inside a session");
        std::process::exit(1);
    }
    // SAFETY: isatty only inspects the fds.
    if unsafe { libc::isatty(0) == 0 || libc::isatty(1) == 0 } {
        eprintln!("stdin and stdout must be a terminal");
        std::process::exit(1);
    }
    match hosts::choose()? {
        Some(server) => client::run(server),
        None => Ok(()),
    }
}
