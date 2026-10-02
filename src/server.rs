//! The server: owns the panes and outlives its clients. Started in the
//! background by the first client, listens on a Unix socket.

use std::fs;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result, bail};

use crate::app::{App, AppEvent};
use crate::protocol::socket_path;
use crate::upgrade;

/// Set once the server is told to stop from outside: SIGTERM or SIGHUP,
/// as a shutdown or logout sends to every process at once.
static STOPPED: AtomicBool = AtomicBool::new(false);

/// True when the server was told to stop from outside. Its shells die in
/// the same moment, which must not look like the session being ended.
pub fn stopped() -> bool {
    STOPPED.load(Ordering::Relaxed)
}

extern "C" fn on_stop(_: libc::c_int) {
    STOPPED.store(true, Ordering::Relaxed);
}

fn catch_stop_signals() {
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            libc::signal(signal, on_stop as *const () as libc::sighandler_t);
        }
    }
}

pub fn run() -> Result<()> {
    catch_stop_signals();
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
    // Where it is now, if the session was renamed.
    if let Ok(now) = socket_path() {
        let _ = fs::remove_file(now);
    }
    result
}
