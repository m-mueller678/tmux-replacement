mod bar;
mod client;
mod menu;
mod paths;
mod protocol;
mod server;
mod session;
mod term;

const USAGE: &str = "\
usage: tmux-replacement [--remote <ssh destination> [--remote-bin <path>]] [--kill-server]

Ctrl-b opens the session picker. With --remote, connects through ssh to the
server on another machine; <path> is the program there (default:
tmux-replacement, looked up in the remote PATH). --kill-server ends the
server and all its sessions, whichever version it is.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["--server"] => server::run(),
        ["--proxy"] => client::proxy(),
        _ => {
            let (server, kill) = parse_client_args(&args).unwrap_or_else(|| {
                eprintln!("{USAGE}");
                std::process::exit(2);
            });
            if kill {
                kill_server(server)
            } else {
                run_client(server)
            }
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn parse_client_args(args: &[String]) -> Option<(client::Server, bool)> {
    let (mut destination, mut bin, mut kill) = (None, None, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--remote" => destination = Some(args.next()?.clone()),
            "--remote-bin" => bin = Some(args.next()?.clone()),
            "--kill-server" => kill = true,
            _ => return None,
        }
    }
    let server = match (destination, bin) {
        (Some(destination), bin) => client::Server::Remote {
            destination,
            bin: bin.unwrap_or_else(|| "tmux-replacement".into()),
        },
        (None, None) => client::Server::Local,
        (None, Some(_)) => return None,
    };
    Some((server, kill))
}

fn kill_server(server: client::Server) -> std::io::Result<()> {
    if client::kill(&server)? {
        eprintln!("server killed");
    } else {
        eprintln!("no server running");
    }
    Ok(())
}

fn run_client(server: client::Server) -> std::io::Result<()> {
    if std::env::var_os("TMUX_REPLACEMENT_SESSION").is_some() {
        eprintln!("already running inside a session");
        std::process::exit(1);
    }
    // SAFETY: isatty only inspects the fds.
    if unsafe { libc::isatty(0) == 0 || libc::isatty(1) == 0 } {
        eprintln!("stdin and stdout must be a terminal");
        std::process::exit(1);
    }
    client::run(server)
}
