//! Messages between the client (the program in the user's terminal) and the
//! server (the background process owning the sessions).
//!
//! Each frame is a tag byte, a little-endian u32 payload length and the
//! payload.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

/// Bumped whenever the protocol changes, so a client never talks to a
/// server from an incompatible build.
pub const VERSION: u32 = 1;

const MAX_FRAME: usize = 16 * 1024 * 1024;

pub enum ClientMsg {
    Hello {
        version: u32,
        size: (u16, u16),
        cwd: PathBuf,
    },
    Input(Vec<u8>),
    Resize((u16, u16)),
}

pub enum ServerMsg {
    /// Bytes to write to the terminal.
    Output(Vec<u8>),
    /// The client should restore its terminal and exit.
    Exit,
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

fn size_bytes((rows, cols): (u16, u16)) -> [u8; 4] {
    let [a, b] = rows.to_le_bytes();
    let [c, d] = cols.to_le_bytes();
    [a, b, c, d]
}

fn parse_size(p: &[u8]) -> io::Result<(u16, u16)> {
    let p: &[u8; 4] = p
        .get(..4)
        .and_then(|p| p.try_into().ok())
        .ok_or_else(invalid)?;
    Ok((
        u16::from_le_bytes([p[0], p[1]]),
        u16::from_le_bytes([p[2], p[3]]),
    ))
}

impl ClientMsg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ClientMsg::Hello { version, size, cwd } => {
                let mut p = version.to_le_bytes().to_vec();
                p.extend_from_slice(&size_bytes(*size));
                p.extend_from_slice(cwd.as_os_str().as_bytes());
                frame(0, &p)
            }
            ClientMsg::Input(bytes) => frame(1, bytes),
            ClientMsg::Resize(size) => frame(2, &size_bytes(*size)),
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<ClientMsg> {
        let (tag, p) = read_frame(r)?;
        Ok(match tag {
            0 => ClientMsg::Hello {
                version: u32::from_le_bytes(p.get(..4).ok_or_else(invalid)?.try_into().unwrap()),
                size: parse_size(&p[4..])?,
                cwd: OsString::from_vec(p[8..].to_vec()).into(),
            },
            1 => ClientMsg::Input(p),
            2 => ClientMsg::Resize(parse_size(&p)?),
            _ => return Err(invalid()),
        })
    }
}

impl ServerMsg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ServerMsg::Output(bytes) => frame(0, bytes),
            ServerMsg::Exit => frame(1, &[]),
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<ServerMsg> {
        let (tag, p) = read_frame(r)?;
        Ok(match tag {
            0 => ServerMsg::Output(p),
            1 => ServerMsg::Exit,
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
            version: 7,
            size: (24, 80),
            cwd: "/tmp/x".into(),
        };
        let mut bytes = hello.encode();
        bytes.extend(ClientMsg::Resize((3, 4)).encode());
        let mut r = &bytes[..];
        match ClientMsg::read(&mut r).unwrap() {
            ClientMsg::Hello { version, size, cwd } => {
                assert_eq!((version, size, cwd), (7, (24, 80), PathBuf::from("/tmp/x")));
            }
            _ => panic!(),
        }
        assert!(matches!(
            ClientMsg::read(&mut r).unwrap(),
            ClientMsg::Resize((3, 4))
        ));
        assert!(ClientMsg::read(&mut r).is_err());
    }
}
