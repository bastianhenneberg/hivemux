//! The server: owns the panes and outlives its clients. Started in the
//! background by the first client, listens on a Unix socket.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result, bail};

use crate::app::{App, AppEvent};
use crate::protocol::socket_path;

pub fn run() -> Result<()> {
    let path = socket_path()?;
    if UnixStream::connect(&path).is_ok() {
        bail!("a hivemux server is already running on {}", path.display());
    }
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to listen on {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;

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

    let result = App::run(tx, &rx, path.clone());
    let _ = fs::remove_file(&path);
    result
}
