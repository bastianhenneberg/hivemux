//! The client: puts the terminal into raw mode, forwards every terminal
//! event to the server and writes whatever the server renders to stdout.

use std::fs::OpenOptions;
use std::io::{self, Write, stdout};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use crossterm::execute;
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};

use crate::protocol::{ClientMsg, Request, ServerMsg, session_socket, socket_path};

/// How a client session ended.
enum Outcome {
    Detached,
    Exited,
    ConnectionLost,
    /// The server sent the client on to another session.
    Switch(String),
}

/// Where the input thread sends terminal events: the connection of the
/// session shown now. Swapped when switching sessions.
type Writer = Arc<Mutex<Option<UnixStream>>>;

/// Attaches to the running server. With `start`, starts one first if there
/// is none.
pub fn run(start: bool) -> Result<()> {
    let path = socket_path()?;
    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(_) if start => start_server(&path, None)?,
        Err(_) => bail!("no hivemux server running"),
    };

    enable_raw_mode()?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        Clear(ClearType::All)
    )?;

    // One input thread for the whole run, writing to whichever session is
    // shown. It stays blocked in `event::read` at the end, which is fine
    // since the process exits right after.
    let writer: Writer = Arc::new(Mutex::new(None));
    let input = Arc::clone(&writer);
    thread::Builder::new().name("input".into()).spawn(move || {
        while let Ok(event) = event::read() {
            if let Some(stream) = input.lock().unwrap().as_mut() {
                let _ = ClientMsg::Event(event).write_to(stream);
            }
        }
    })?;

    let mut stream = stream;
    let outcome = loop {
        match session(stream, &writer) {
            Ok(Outcome::Switch(name)) => {
                let path = session_socket(&name)?;
                stream = match UnixStream::connect(&path) {
                    Ok(stream) => stream,
                    Err(_) => start_server(&path, Some(&name))?,
                };
                execute!(stdout(), Clear(ClearType::All))?;
            }
            other => break other,
        }
    };

    let _ = execute!(
        stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
    let _ = disable_raw_mode();

    match outcome? {
        Outcome::Detached => println!("[detached]"),
        Outcome::Exited => println!("[exited]"),
        Outcome::ConnectionLost => println!("[lost connection to server]"),
        Outcome::Switch(_) => {}
    }
    Ok(())
}

/// Tells a running server to shut down.
pub fn kill_server() -> Result<()> {
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path).context("no hivemux server running")?;
    ClientMsg::KillServer.write_to(&mut stream)?;
    // Wait until the server has closed every shell and let go of the
    // socket, so `hivemux kill-server && hivemux` starts a new one instead
    // of reaching the old one on its way out.
    for _ in 0..150 {
        if UnixStream::connect(&path).is_err() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    bail!("the server did not shut down within 3 seconds")
}

/// Sends `request` to the running server and returns its answer.
pub fn request(request: Request) -> Result<serde_json::Value> {
    request_at(&socket_path()?, request)
}

/// Sends `request` to the server at `path`.
pub fn request_at(path: &Path, request: Request) -> Result<serde_json::Value> {
    let mut stream = UnixStream::connect(path).context("no hivemux server running")?;
    ClientMsg::Request(request).write_to(&mut stream)?;
    loop {
        match ServerMsg::read_from(&mut stream)? {
            Some(ServerMsg::Reply(Ok(value))) => return Ok(value),
            Some(ServerMsg::Reply(Err(message))) => bail!(message),
            Some(_) => {}
            None => bail!("the server closed the connection"),
        }
    }
}

fn session(mut stream: UnixStream, writer: &Writer) -> Result<Outcome> {
    let mut to_server = stream.try_clone()?;
    let (cols, rows) = terminal::size()?;
    ClientMsg::Attach.write_to(&mut to_server)?;
    ClientMsg::Event(Event::Resize(cols, rows)).write_to(&mut to_server)?;
    *writer.lock().unwrap() = Some(to_server);

    let mut out = stdout().lock();
    loop {
        match ServerMsg::read_from(&mut stream) {
            Ok(Some(ServerMsg::Output(bytes))) => {
                out.write_all(&bytes)?;
                out.flush()?;
            }
            Ok(Some(ServerMsg::Detached)) => return Ok(Outcome::Detached),
            Ok(Some(ServerMsg::Exited)) => return Ok(Outcome::Exited),
            Ok(Some(ServerMsg::Reply(_))) => {}
            Ok(Some(ServerMsg::Switch(name))) => return Ok(Outcome::Switch(name)),
            Ok(None) | Err(_) => return Ok(Outcome::ConnectionLost),
        }
    }
}

/// Starts a server in the background and connects to it. With `session`,
/// the server is that named session, whatever this process was started for.
fn start_server(path: &Path, session: Option<&str>) -> Result<UnixStream> {
    // A socket file without a server behind it is left over from a crash.
    let _ = std::fs::remove_file(path);

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path.with_extension("log"))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    if let Some(name) = session {
        cmd.env("HIVEMUX_SESSION", name)
            .env_remove("HIVEMUX_SOCKET");
    }
    cmd.arg("server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe. A new session detaches the server
    // from this terminal, so closing the terminal does not kill it.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().context("failed to start the hivemux server")?;

    for _ in 0..100 {
        if let Ok(stream) = UnixStream::connect(path) {
            return Ok(stream);
        }
        thread::sleep(Duration::from_millis(20));
    }
    bail!(
        "the hivemux server did not start, see {}",
        path.with_extension("log").display()
    )
}
