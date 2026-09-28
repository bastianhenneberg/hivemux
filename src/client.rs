//! The client: puts the terminal into raw mode, forwards every terminal
//! event to the server and writes whatever the server renders to stdout.

use std::fs::OpenOptions;
use std::io::{self, Write, stdout};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
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

use crate::protocol::{ClientMsg, Request, ServerMsg, socket_path};

/// How a client session ended.
enum Outcome {
    Detached,
    Exited,
    ConnectionLost,
}

/// Attaches to the running server. With `start`, starts one first if there
/// is none.
pub fn run(start: bool) -> Result<()> {
    let path = socket_path()?;
    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(_) if start => start_server(&path)?,
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

    let outcome = session(stream);

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
    }
    Ok(())
}

/// Tells a running server to shut down.
pub fn kill_server() -> Result<()> {
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path).context("no hivemux server running")?;
    ClientMsg::KillServer.write_to(&mut stream)?;
    Ok(())
}

/// Sends `request` to the running server and returns its answer.
pub fn request(request: Request) -> Result<serde_json::Value> {
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path).context("no hivemux server running")?;
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

fn session(mut stream: UnixStream) -> Result<Outcome> {
    let mut writer = stream.try_clone()?;
    let (cols, rows) = terminal::size()?;
    ClientMsg::Attach.write_to(&mut writer)?;
    ClientMsg::Event(Event::Resize(cols, rows)).write_to(&mut writer)?;

    // Input is forwarded on its own thread. It stays blocked in
    // `event::read` when the session ends, which is fine since the process
    // exits right after.
    thread::Builder::new().name("input".into()).spawn(move || {
        while let Ok(event) = event::read() {
            if ClientMsg::Event(event).write_to(&mut writer).is_err() {
                break;
            }
        }
    })?;

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
            Ok(None) | Err(_) => return Ok(Outcome::ConnectionLost),
        }
    }
}

/// Starts a server in the background and connects to it.
fn start_server(path: &Path) -> Result<UnixStream> {
    // A socket file without a server behind it is left over from a crash.
    let _ = std::fs::remove_file(path);

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path.with_extension("log"))?;
    let mut cmd = Command::new(std::env::current_exe()?);
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
