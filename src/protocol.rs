//! Messages between the client (the program in the user's terminal) and the
//! server (the background process owning the sessions).
//!
//! Each frame is a tag byte, a little-endian u32 payload length and the
//! payload.
//!
//! So that any client can recognize and replace a server from another
//! build, these parts never change:
//! - the frame layout;
//! - the client's Hello (tag 0) starts with the protocol version as a u32;
//! - a server answers a Hello with another version by [`ServerMsg::Mismatch`]
//!   (tag 255: u32 protocol version, then its package version as a u32
//!   length and UTF-8 bytes) and closes the connection;
//! - a server receiving [`ClientMsg::KillServer`] (tag 255, empty) as the
//!   first or any later frame ends its sessions and exits, closing the
//!   connection only once it is gone.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

/// Bumped whenever the protocol changes, so a client never talks to a
/// server from an incompatible build.
pub const VERSION: u32 = 4;

/// The version of this build, for telling the user which servers mismatch.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

const STABLE_TAG: u8 = 255;

const MAX_FRAME: usize = 16 * 1024 * 1024;

pub enum ClientMsg {
    Hello {
        version: u32,
        size: (u16, u16),
        cwd: PathBuf,
        /// Identifies the client across reconnects, so the server can drop
        /// the connection a reconnecting client left behind.
        token: u64,
        /// Session to attach to right away instead of opening the picker.
        resume: Option<String>,
    },
    Input(Vec<u8>),
    Resize((u16, u16)),
    /// Ends the server and all its sessions. Stable across versions.
    KillServer,
}

pub enum ServerMsg {
    /// Bytes to write to the terminal that belong to no session: the picker,
    /// forwarded bells, cleanup.
    Output(Vec<u8>),
    /// Bytes to write to the terminal that are part of the viewed session's
    /// display.
    SessionOutput(Vec<u8>),
    /// The client now shows the named session, or the picker for None. If
    /// `fresh`, the session output that follows starts with a full repaint.
    View {
        session: Option<String>,
        fresh: bool,
    },
    /// Names of all sessions.
    Sessions(Vec<String>),
    /// The name of the machine the server runs on, sent first.
    Host(String),
    /// The client should restore its terminal and exit.
    Exit,
    /// Answers a Hello with a different protocol version. Stable across
    /// versions.
    Mismatch { protocol: u32, version: String },
}

fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(tag);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header[1..].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed message")
}

/// Appends fields to a payload.
#[derive(Default)]
struct Fields(Vec<u8>);

impl Fields {
    fn u16(mut self, n: u16) -> Self {
        self.0.extend_from_slice(&n.to_le_bytes());
        self
    }
    fn u32(mut self, n: u32) -> Self {
        self.0.extend_from_slice(&n.to_le_bytes());
        self
    }
    fn u64(mut self, n: u64) -> Self {
        self.0.extend_from_slice(&n.to_le_bytes());
        self
    }
    fn bytes(self, b: &[u8]) -> Self {
        let mut s = self.u32(b.len() as u32);
        s.0.extend_from_slice(b);
        s
    }
    fn opt(self, b: Option<&[u8]>) -> Self {
        match b {
            Some(b) => self.u16(1).bytes(b),
            None => self.u16(0),
        }
    }
}

/// Reads fields from a payload.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>().ok_or_else(invalid)?;
        self.0 = rest;
        Ok(*head)
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take()?))
    }
    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let len = self.u32()? as usize;
        if len > self.0.len() {
            return Err(invalid());
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(head.to_vec())
    }
    fn string(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| invalid())
    }
    fn opt_string(&mut self) -> io::Result<Option<String>> {
        Ok(match self.u16()? {
            0 => None,
            _ => Some(self.string()?),
        })
    }
}

impl ClientMsg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ClientMsg::Hello {
                version,
                size,
                cwd,
                token,
                resume,
            } => {
                let p = Fields::default()
                    .u32(*version)
                    .u16(size.0)
                    .u16(size.1)
                    .bytes(cwd.as_os_str().as_bytes())
                    .u64(*token)
                    .opt(resume.as_deref().map(str::as_bytes));
                frame(0, &p.0)
            }
            ClientMsg::Input(bytes) => frame(1, bytes),
            ClientMsg::Resize((rows, cols)) => frame(2, &Fields::default().u16(*rows).u16(*cols).0),
            ClientMsg::KillServer => frame(STABLE_TAG, &[]),
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<ClientMsg> {
        let (tag, p) = read_frame(r)?;
        let mut c = Cursor(&p);
        Ok(match tag {
            0 => {
                // Check the version first: the rest may be laid out differently.
                let version = c.u32()?;
                if version != VERSION {
                    return Ok(ClientMsg::Hello {
                        version,
                        size: (0, 0),
                        cwd: PathBuf::new(),
                        token: 0,
                        resume: None,
                    });
                }
                ClientMsg::Hello {
                    version,
                    size: (c.u16()?, c.u16()?),
                    cwd: OsString::from_vec(c.bytes()?).into(),
                    token: c.u64()?,
                    resume: c.opt_string()?,
                }
            }
            1 => ClientMsg::Input(p),
            2 => ClientMsg::Resize((c.u16()?, c.u16()?)),
            STABLE_TAG => ClientMsg::KillServer,
            _ => return Err(invalid()),
        })
    }
}

impl ServerMsg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ServerMsg::Output(bytes) => frame(0, bytes),
            ServerMsg::Exit => frame(1, &[]),
            ServerMsg::SessionOutput(bytes) => frame(2, bytes),
            ServerMsg::View { session, fresh } => {
                let p = Fields::default()
                    .u16(*fresh as u16)
                    .opt(session.as_deref().map(str::as_bytes));
                frame(3, &p.0)
            }
            ServerMsg::Sessions(names) => {
                let p = names
                    .iter()
                    .fold(Fields::default().u32(names.len() as u32), |p, n| {
                        p.bytes(n.as_bytes())
                    });
                frame(4, &p.0)
            }
            ServerMsg::Host(name) => frame(5, name.as_bytes()),
            ServerMsg::Mismatch { protocol, version } => frame(
                STABLE_TAG,
                &Fields::default().u32(*protocol).bytes(version.as_bytes()).0,
            ),
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<ServerMsg> {
        let (tag, p) = read_frame(r)?;
        let mut c = Cursor(&p);
        Ok(match tag {
            0 => ServerMsg::Output(p),
            1 => ServerMsg::Exit,
            2 => ServerMsg::SessionOutput(p),
            3 => ServerMsg::View {
                fresh: c.u16()? != 0,
                session: c.opt_string()?,
            },
            4 => {
                let n = c.u32()?;
                ServerMsg::Sessions((0..n).map(|_| c.string()).collect::<io::Result<_>>()?)
            }
            5 => ServerMsg::Host(String::from_utf8(p).map_err(|_| invalid())?),
            STABLE_TAG => ServerMsg::Mismatch {
                protocol: c.u32()?,
                version: c.string()?,
            },
            _ => return Err(invalid()),
        })
    }
}

pub fn send(w: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    w.write_all(frame)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let hello = ClientMsg::Hello {
            version: VERSION,
            size: (24, 80),
            cwd: "/tmp/x".into(),
            token: 99,
            resume: Some("work".into()),
        };
        let mut bytes = hello.encode();
        bytes.extend(ClientMsg::Resize((3, 4)).encode());
        bytes.extend(ClientMsg::KillServer.encode());
        let mut r = &bytes[..];
        match ClientMsg::read(&mut r).unwrap() {
            ClientMsg::Hello {
                version,
                size,
                cwd,
                token,
                resume,
            } => {
                assert_eq!((version, size, token), (VERSION, (24, 80), 99));
                assert_eq!(
                    (cwd, resume.as_deref()),
                    (PathBuf::from("/tmp/x"), Some("work"))
                );
            }
            _ => panic!(),
        }
        assert!(matches!(
            ClientMsg::read(&mut r).unwrap(),
            ClientMsg::Resize((3, 4))
        ));
        assert!(matches!(
            ClientMsg::read(&mut r).unwrap(),
            ClientMsg::KillServer
        ));
        assert!(ClientMsg::read(&mut r).is_err());
        // The stable frames, byte for byte.
        assert_eq!(ClientMsg::KillServer.encode(), [255, 0, 0, 0, 0]);
        let mismatch = ServerMsg::Mismatch {
            protocol: 7,
            version: "1.2".into(),
        };
        assert_eq!(
            mismatch.encode(),
            [255, 11, 0, 0, 0, 7, 0, 0, 0, 3, 0, 0, 0, b'1', b'.', b'2']
        );

        let mut bytes = ServerMsg::View {
            session: Some("a".into()),
            fresh: true,
        }
        .encode();
        bytes.extend(ServerMsg::Sessions(vec!["a".into(), "b".into()]).encode());
        bytes.extend(ServerMsg::Host("box".into()).encode());
        let mut r = &bytes[..];
        assert!(matches!(
            ServerMsg::read(&mut r).unwrap(),
            ServerMsg::View { session: Some(s), fresh: true } if s == "a"
        ));
        assert!(
            matches!(ServerMsg::read(&mut r).unwrap(), ServerMsg::Sessions(n) if n == ["a", "b"])
        );
        assert!(matches!(ServerMsg::read(&mut r).unwrap(), ServerMsg::Host(h) if h == "box"));
    }
}
