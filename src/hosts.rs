//! The picker shown at startup to choose the machine whose sessions to use:
//! this one, or any host from the user's ssh config.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::client::Server;
use crate::menu::{Item, Menu, Outcome, Target};
use crate::term;

/// The host name up to the first dot, as tmux shows it.
pub fn short_hostname() -> String {
    let name = nix::unistd::gethostname().unwrap_or_default();
    let name = name.to_string_lossy();
    name.split('.').next().unwrap_or_default().to_string()
}

/// Asks which machine to connect to; None if the user cancels.
pub fn choose() -> io::Result<Option<Server>> {
    let mut names = vec![short_hostname(), "localhost".to_string()];
    let local = names.len();
    if let Some(home) = std::env::var_os("HOME") {
        for host in ssh_hosts(Path::new(&home)) {
            if !names.contains(&host) {
                names.push(host);
            }
        }
    }
    let items = names
        .iter()
        .enumerate()
        .map(|(i, name)| Item {
            label: name.clone(),
            target: Target::Session(i as u64),
            note: Some(if i < local { "this machine" } else { "ssh" }.into()),
            alert: false,
        })
        .collect();
    let mut menu = Menu::new(None, items);

    term::enable_raw_mode().map_err(io::Error::from)?;
    term::write(format!("\x1b[?1049h{}", term::MODE_RESET).as_bytes());
    let choice = pick(&mut menu);
    term::write(b"\x1b[?1049l");
    term::restore_mode();

    Ok(choice?.map(|i| {
        if i < local {
            Server::Local
        } else {
            Server::Remote {
                destination: names[i].clone(),
            }
        }
    }))
}

fn pick(menu: &mut Menu) -> io::Result<Option<usize>> {
    let mut buf = [0u8; 4096];
    loop {
        // Rendering on every key also catches up with resizes.
        term::write(menu.render(term::size()).as_bytes());
        let n = io::stdin().read(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        let mut input = &buf[..n];
        while !input.is_empty() {
            let (outcome, consumed) = menu.handle_input(input);
            input = &input[consumed..];
            match outcome {
                Outcome::Select(Target::Session(i)) => return Ok(Some(i as usize)),
                Outcome::Cancel => return Ok(None),
                _ => {}
            }
        }
    }
}

/// Host aliases from the user's ssh config and the files it includes, in
/// order, leaving out patterns.
pub fn ssh_hosts(home: &Path) -> Vec<String> {
    let mut hosts = Vec::new();
    read_config(&home.join(".ssh/config"), home, &mut hosts, 0);
    hosts
}

fn read_config(path: &Path, home: &Path, hosts: &mut Vec<String>, depth: u32) {
    // ssh itself refuses deeper include chains.
    if depth > 16 {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        // Keywords are case-insensitive and may be followed by `=`.
        let (keyword, rest) = line
            .split_once(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or((line, ""));
        let args = rest
            .trim_start_matches(|c: char| c.is_whitespace() || c == '=')
            .split_whitespace()
            .map(|a| a.trim_matches('"'));
        match keyword.to_ascii_lowercase().as_str() {
            "host" => {
                for host in args {
                    let pattern = host.contains(['*', '?', '!']);
                    if !pattern && !hosts.iter().any(|h| h == host) {
                        hosts.push(host.to_string());
                    }
                }
            }
            "include" => {
                for include in args {
                    for file in expand_include(include, home) {
                        read_config(&file, home, hosts, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Resolves an Include argument as ssh does: relative to ~/.ssh, with `~`
/// and wildcards in the file name.
fn expand_include(include: &str, home: &Path) -> Vec<PathBuf> {
    let path = match include.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => home.join(".ssh").join(include),
    };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    let (Some(dir), Some(name)) = (path.parent(), name) else {
        return Vec::new();
    };
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| wildcard(name.as_bytes(), e.file_name().as_encoded_bytes()))
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

fn wildcard(pattern: &[u8], text: &[u8]) -> bool {
    match (pattern.split_first(), text.split_first()) {
        (None, None) => true,
        (Some((b'*', rest)), _) => {
            wildcard(rest, text) || (!text.is_empty() && wildcard(pattern, &text[1..]))
        }
        (Some((b'?', p)), Some((_, t))) => wildcard(p, t),
        (Some((a, p)), Some((b, t))) if a == b => wildcard(p, t),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ssh_config() {
        let home = std::env::temp_dir().join(format!("hosts-test-{}", std::process::id()));
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(ssh.join("config.d")).unwrap();
        std::fs::write(
            ssh.join("config"),
            "# comment\nHost box web\n  User me\nhost=*.lan !bad\nInclude config.d/*.conf\n\
             Host \"quoted\" box\nMatch host x\nInclude ~/.ssh/missing\n",
        )
        .unwrap();
        std::fs::write(ssh.join("config.d/b.conf"), "Host bee\n").unwrap();
        std::fs::write(ssh.join("config.d/a.conf"), "HOST ay\n").unwrap();
        std::fs::write(ssh.join("config.d/c.txt"), "Host no\n").unwrap();
        let hosts = ssh_hosts(&home);
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(hosts, ["box", "web", "ay", "bee", "quoted"]);
    }

    #[test]
    fn matches_wildcards() {
        assert!(wildcard(b"*.conf", b"a.conf"));
        assert!(wildcard(b"a?c*", b"abcdef"));
        assert!(!wildcard(b"*.conf", b"a.txt"));
    }
}
