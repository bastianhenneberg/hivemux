//! The wire protocol between client and server, spoken over a Unix socket.
//!
//! Every message is one frame: a tag byte, the payload length as a 4 byte
//! big-endian integer, then the payload.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result};
use crossterm::event::Event;
use serde::{Deserialize, Serialize};

use crate::agent::AgentState;
use crate::layout::PaneId;

/// Frames larger than this are treated as a broken stream.
const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Client to server.
#[derive(Debug, PartialEq)]
pub enum ClientMsg {
    /// A terminal event: key, paste, resize, ...
    Event(Event),
    /// Shut the server down, killing all panes.
    KillServer,
    /// Become the attached client, showing the session. A connection that
    /// does not send this only makes requests.
    Attach,
    /// A command from the CLI or an agent, answered with `ServerMsg::Reply`.
    Request(Request),
}

/// Commands for scripts and agents, sent by `hivemux status`, `list`,
/// `send` and `new`.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    /// Set a pane's agent state, or clear it with `None`.
    Status {
        pane: PaneId,
        state: Option<AgentState>,
    },
    /// All panes.
    List,
    /// Type `text` into a pane, followed by Enter if `enter`.
    Send {
        pane: PaneId,
        text: String,
        enter: bool,
    },
    /// Start `command` (the shell if empty) in a new pane.
    New {
        command: Vec<String>,
        float: bool,
        workspace: Option<u8>,
        cwd: Option<PathBuf>,
    },
}

/// The answer to a request: JSON on success, a message on failure.
pub type Reply = std::result::Result<serde_json::Value, String>;

/// Server to client.
#[derive(Debug, PartialEq)]
pub enum ServerMsg {
    /// Bytes to write to the client's terminal as they are.
    Output(Vec<u8>),
    /// The client was detached, the server keeps running.
    Detached,
    /// The server is shutting down.
    Exited,
    /// The answer to a `ClientMsg::Request`.
    Reply(Reply),
}

impl ClientMsg {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            ClientMsg::Event(event) => write_frame(w, 1, &serde_json::to_vec(event)?),
            ClientMsg::KillServer => write_frame(w, 2, &[]),
            ClientMsg::Attach => write_frame(w, 3, &[]),
            ClientMsg::Request(request) => write_frame(w, 4, &serde_json::to_vec(request)?),
        }
    }

    /// Reads the next message, `None` at the end of the stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Self>> {
        let Some((tag, payload)) = read_frame(r)? else {
            return Ok(None);
        };
        match tag {
            1 => Ok(Some(ClientMsg::Event(serde_json::from_slice(&payload)?))),
            2 => Ok(Some(ClientMsg::KillServer)),
            3 => Ok(Some(ClientMsg::Attach)),
            4 => Ok(Some(ClientMsg::Request(serde_json::from_slice(&payload)?))),
            _ => Err(invalid(format!("unknown client message {tag}"))),
        }
    }
}

impl ServerMsg {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            ServerMsg::Output(bytes) => write_frame(w, 1, bytes),
            ServerMsg::Detached => write_frame(w, 2, &[]),
            ServerMsg::Exited => write_frame(w, 3, &[]),
            ServerMsg::Reply(reply) => write_frame(w, 4, &serde_json::to_vec(reply)?),
        }
    }

    /// Reads the next message, `None` at the end of the stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Self>> {
        let Some((tag, payload)) = read_frame(r)? else {
            return Ok(None);
        };
        match tag {
            1 => Ok(Some(ServerMsg::Output(payload))),
            2 => Ok(Some(ServerMsg::Detached)),
            3 => Ok(Some(ServerMsg::Exited)),
            4 => Ok(Some(ServerMsg::Reply(serde_json::from_slice(&payload)?))),
            _ => Err(invalid(format!("unknown server message {tag}"))),
        }
    }
}

fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| invalid("frame too large".into()))?;
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(tag);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(payload);
    w.write_all(&frame)?;
    w.flush()
}

fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME {
        return Err(invalid(format!("frame of {len} bytes is too large")));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((header[0], payload)))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// The server socket: `$HIVEMUX_SOCKET` if set, otherwise
/// `$XDG_RUNTIME_DIR/hivemux/default.sock`, falling back to
/// `/tmp/hivemux-<uid>/default.sock`. Creates the directory, readable only by
/// the current user.
pub fn socket_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("HIVEMUX_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => PathBuf::from(runtime).join("hivemux"),
        // SAFETY: getuid has no preconditions and cannot fail.
        None => PathBuf::from(format!("/tmp/hivemux-{}", unsafe { libc::getuid() })),
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir.join("default.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn client_messages_round_trip() {
        let msgs = [
            ClientMsg::Event(Event::Key(KeyEvent::new(
                KeyCode::Char('b'),
                KeyModifiers::CONTROL,
            ))),
            ClientMsg::Event(Event::Resize(120, 40)),
            ClientMsg::Event(Event::Paste("hallo\nwelt".into())),
            ClientMsg::KillServer,
            ClientMsg::Attach,
            ClientMsg::Request(Request::Status {
                pane: 3,
                state: Some(AgentState::Blocked),
            }),
            ClientMsg::Request(Request::New {
                command: vec!["claude".into(), "--resume".into()],
                float: true,
                workspace: Some(2),
                cwd: None,
            }),
        ];
        let mut wire = Vec::new();
        for msg in &msgs {
            msg.write_to(&mut wire).unwrap();
        }
        let mut r = wire.as_slice();
        for msg in msgs {
            assert_eq!(ClientMsg::read_from(&mut r).unwrap(), Some(msg));
        }
        assert_eq!(ClientMsg::read_from(&mut r).unwrap(), None);
    }

    #[test]
    fn server_messages_round_trip() {
        let msgs = [
            ServerMsg::Output(b"\x1b[2J\x1b[Hhi".to_vec()),
            ServerMsg::Output(Vec::new()),
            ServerMsg::Detached,
            ServerMsg::Exited,
            ServerMsg::Reply(Ok(serde_json::json!({"pane": 4}))),
            ServerMsg::Reply(Err("no pane 9".into())),
        ];
        let mut wire = Vec::new();
        for msg in &msgs {
            msg.write_to(&mut wire).unwrap();
        }
        let mut r = wire.as_slice();
        for msg in msgs {
            assert_eq!(ServerMsg::read_from(&mut r).unwrap(), Some(msg));
        }
        assert_eq!(ServerMsg::read_from(&mut r).unwrap(), None);
    }

    #[test]
    fn truncated_frame_is_an_error_not_a_clean_end() {
        let mut wire = Vec::new();
        ServerMsg::Output(b"hello".to_vec())
            .write_to(&mut wire)
            .unwrap();
        wire.truncate(7);
        assert!(ServerMsg::read_from(&mut wire.as_slice()).is_err());
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let wire = [1u8, 0xff, 0xff, 0xff, 0xff];
        assert!(ServerMsg::read_from(&mut wire.as_slice()).is_err());
    }
}
