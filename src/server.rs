//! The server: owns the panes and outlives its clients. Started in the
//! background by the first client, listens on a Unix socket.

use std::fs;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result, bail};

use crate::app::{App, AppEvent};
use crate::protocol::socket_path;
use crate::upgrade;

pub fn run() -> Result<()> {
    let path = socket_path()?;
    // After `hivemux update` the socket is open already.
    let handover = upgrade::take();
    let listener = match &handover {
        Some(handover) => {
            upgrade::set_cloexec(handover.listener, true)?;
            // SAFETY: the server before handed its listener over.
            unsafe { UnixListener::from_raw_fd(handover.listener) }
        }
        None => {
            if UnixStream::connect(&path).is_ok() {
                bail!("a hivemux server is already running on {}", path.display());
            }
            let _ = fs::remove_file(&path);
            let listener = UnixListener::bind(&path)
                .with_context(|| format!("failed to listen on {}", path.display()))?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            listener
        }
    };
    let listener_fd = listener.as_raw_fd();

    let (tx, rx) = mpsc::channel();
    let accept_tx = tx.clone();
    thread::Builder::new()
        .name("accept".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if accept_tx.send(AppEvent::ClientConnected(stream)).is_err() {
                    break;
                }
            }
        })?;

    let result = App::run(tx, &rx, path.clone(), listener_fd, handover);
    let _ = fs::remove_file(&path);
    result
}
