mod bar;
mod client;
mod menu;
mod paths;
mod protocol;
mod server;
mod session;
mod term;

const USAGE: &str = "\
usage: tmux-replacement [--remote <ssh destination> [--remote-bin <path>]]

Ctrl-b opens the session picker. With --remote, connects through ssh to the
server on another machine; <path> is the program there (default:
tmux-replacement, looked up in the remote PATH).";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["--server"] => server::run(),
        ["--proxy"] => client::proxy(),
        [] => run_client(client::Server::Local),
        ["--remote", destination] => run_client(client::Server::Remote {
            destination: destination.into(),
            bin: "tmux-replacement".into(),
        }),
        ["--remote", destination, "--remote-bin", bin]
        | ["--remote-bin", bin, "--remote", destination] => run_client(client::Server::Remote {
            destination: destination.into(),
            bin: bin.into(),
        }),
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
