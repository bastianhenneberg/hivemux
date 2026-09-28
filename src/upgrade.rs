//! `hivemux update`: the server replaces itself with a new binary and the
//! session goes on. It execs the new binary in its own process, which keeps
//! every pane's pty, the socket and the attached client's connection open,
//! and hands over the rest in a file. The shells and agents never notice,
//! they are still the children of the same process.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use serde::{Deserialize, Serialize};

use crate::agent::AgentState;
use crate::layout::PaneId;
use crate::persist::Saved;
use crate::protocol::Caps;

/// Set for the new binary: the path of the handover file.
const ENV: &str = "HIVEMUX_UPGRADE";

/// Everything the new server needs besides the open file descriptors.
#[derive(Serialize, Deserialize)]
pub struct Handover {
    pub listener: RawFd,
    /// The attached client's connection.
    pub client: Option<RawFd>,
    pub screen: (u16, u16),
    pub caps: Caps,
    pub focus_mode: bool,
    pub saved: Saved,
    pub panes: BTreeMap<PaneId, HandoverPane>,
}

#[derive(Serialize, Deserialize)]
pub struct HandoverPane {
    /// The master side of the pane's pty.
    pub fd: RawFd,
    pub pid: u32,
    pub rows: u16,
    pub cols: u16,
    /// The history as text and the screen as escape codes, to fill the new
    /// terminal state with.
    pub history: String,
    pub screen: Vec<u8>,
    pub reported: Option<AgentState>,
}

/// The handover left by the server this process was before, if any. The
/// file is removed, so a crash later does not pick it up again.
pub fn take() -> Option<Handover> {
    let path = PathBuf::from(std::env::var_os(ENV)?);
    // SAFETY: called first thing in the server, before any thread starts.
    unsafe { std::env::remove_var(ENV) };
    let text = fs::read_to_string(&path);
    let _ = fs::remove_file(&path);
    match text
        .map_err(anyhow::Error::from)
        .and_then(|text| serde_json::from_str(&text).context("the handover file is broken"))
    {
        Ok(handover) => Some(handover),
        Err(e) => {
            eprintln!("update: cannot read {}: {e:#}", path.display());
            None
        }
    }
}

/// Writes `handover` next to the socket, lets its descriptors survive the
/// exec and becomes `exe`. Returns only when that fails, with everything
/// as it was.
pub fn exec(exe: &Path, socket: &Path, handover: &Handover) -> anyhow::Error {
    let path = socket.with_extension("upgrade.json");
    let fds: Vec<RawFd> = handover
        .panes
        .values()
        .map(|p| p.fd)
        .chain([handover.listener])
        .chain(handover.client)
        .collect();
    let result = (|| -> Result<()> {
        if !exe.is_file() {
            bail!("{} does not exist", exe.display());
        }
        let json = serde_json::to_vec(handover)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(&json)?;
        for &fd in &fds {
            set_cloexec(fd, false)?;
        }
        Ok(())
    })();
    let error = match result {
        Ok(()) => anyhow::Error::from(
            Command::new(exe)
                .arg("server")
                .env(ENV, &path)
                // It may have been renamed since it started.
                .env("HIVEMUX_SESSION", crate::protocol::session_name())
                .exec(),
        )
        .context(format!("cannot run {}", exe.display())),
        Err(e) => e,
    };
    for &fd in &fds {
        let _ = set_cloexec(fd, true);
    }
    let _ = fs::remove_file(&path);
    error
}

pub fn set_cloexec(fd: RawFd, on: bool) -> io::Result<()> {
    // SAFETY: plain fcntl on a descriptor this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = if on {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        if libc::fcntl(fd, libc::F_SETFD, flags) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A pty master taken over from the server before, which portable-pty
/// cannot open from a descriptor.
pub struct AdoptedMaster {
    file: File,
}

impl AdoptedMaster {
    /// # Safety
    /// `fd` must be an open pty master that nothing else owns.
    pub unsafe fn from_raw_fd(fd: RawFd) -> Self {
        // Descriptors of this process close on the next exec, as those
        // portable-pty opens do.
        let _ = set_cloexec(fd, true);
        Self {
            // SAFETY: see above.
            file: unsafe { File::from_raw_fd(fd) },
        }
    }
}

impl MasterPty for AdoptedMaster {
    fn resize(&self, size: PtySize) -> Result<(), anyhow::Error> {
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        // SAFETY: TIOCSWINSZ reads a winsize.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCSWINSZ, &ws) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn get_size(&self) -> Result<PtySize, anyhow::Error> {
        // SAFETY: TIOCGWINSZ writes a winsize, which is plain data.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(PtySize {
            rows: ws.ws_row,
            cols: ws.ws_col,
            pixel_width: ws.ws_xpixel,
            pixel_height: ws.ws_ypixel,
        })
    }

    fn try_clone_reader(&self) -> Result<Box<dyn io::Read + Send>, anyhow::Error> {
        Ok(Box::new(self.file.try_clone()?))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>, anyhow::Error> {
        Ok(Box::new(self.file.try_clone()?))
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        // SAFETY: tcgetpgrp only reads.
        match unsafe { libc::tcgetpgrp(self.file.as_raw_fd()) } {
            pid if pid > 0 => Some(pid),
            _ => None,
        }
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.file.as_raw_fd())
    }

    fn tty_name(&self) -> Option<PathBuf> {
        None
    }
}

/// A pane's process started by the server before. It is still a child of
/// this process, the exec kept the pid.
#[derive(Debug, Clone)]
pub struct AdoptedChild {
    pid: u32,
}

impl AdoptedChild {
    pub fn new(pid: u32) -> Self {
        Self { pid }
    }

    fn wait_pid(&self, flags: libc::c_int) -> io::Result<Option<ExitStatus>> {
        let mut status = 0;
        // SAFETY: waitpid on our own child.
        match unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, flags) } {
            0 => Ok(None),
            -1 => {
                let e = io::Error::last_os_error();
                // Reaped already, e.g. by an earlier wait.
                if e.raw_os_error() == Some(libc::ECHILD) {
                    Ok(Some(ExitStatus::with_exit_code(0)))
                } else {
                    Err(e)
                }
            }
            _ if libc::WIFEXITED(status) => Ok(Some(ExitStatus::with_exit_code(
                libc::WEXITSTATUS(status) as u32,
            ))),
            _ => Ok(Some(ExitStatus::with_exit_code(1))),
        }
    }
}

impl ChildKiller for AdoptedChild {
    /// SIGHUP, as closing a terminal does, and SIGKILL if that does not end
    /// it within a moment. Same as portable-pty.
    fn kill(&mut self) -> io::Result<()> {
        let pid = self.pid as libc::pid_t;
        // SAFETY: signals to our own child.
        if unsafe { libc::kill(pid, libc::SIGHUP) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for attempt in 0..5 {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(50));
            }
            if let Ok(Some(_)) = self.try_wait() {
                return Ok(());
            }
        }
        // SAFETY: as above.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

impl Child for AdoptedChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_pid(libc::WNOHANG)
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        Ok(self
            .wait_pid(0)?
            .unwrap_or_else(|| ExitStatus::with_exit_code(0)))
    }

    fn process_id(&self) -> Option<u32> {
        Some(self.pid)
    }
}

/// The bytes that bring a fresh terminal state back to `history` above the
/// screen `screen`: the history lines, pushed off a screen of `rows` rows,
/// then the screen itself.
pub fn replay(history: &str, screen: &[u8], rows: u16) -> Vec<u8> {
    let mut out = Vec::new();
    if !history.is_empty() {
        for line in history.split('\n') {
            out.extend_from_slice(line.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        // Every history line off the screen, and not one more.
        out.extend(std::iter::repeat_n(
            b'\n',
            usize::from(rows.saturating_sub(1)),
        ));
    }
    out.extend_from_slice(b"\x1b[H\x1b[2J");
    out.extend_from_slice(screen);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_restores_history_and_screen() {
        let mut old = vt100::Parser::new(3, 20, 100);
        for i in 0..8 {
            old.process(format!("line {i}\r\n").as_bytes());
        }
        old.process(b"\x1b[31mred\x1b[m prompt");

        // What the server hands over: the history as text, the screen as
        // escape codes.
        old.screen_mut().set_scrollback(usize::MAX);
        let history_len = old.screen().scrollback();
        old.screen_mut().set_scrollback(0);
        let history: Vec<String> = (0..history_len).map(|i| format!("line {i}")).collect();
        let screen = old.screen().state_formatted();

        let mut new = vt100::Parser::new(3, 20, 100);
        new.process(&replay(&history.join("\n"), &screen, 3));
        assert_eq!(new.screen().contents(), old.screen().contents());
        assert_eq!(
            new.screen().cursor_position(),
            old.screen().cursor_position()
        );
        assert_eq!(
            new.screen().cell(2, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
        new.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(new.screen().scrollback(), history_len);
        assert!(new.screen().contents().starts_with("line 0"));
    }

    #[test]
    fn adopted_child_waits_and_kills() {
        // Waited on through the adopted child, not this handle.
        #[allow(clippy::zombie_processes)]
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let mut adopted = AdoptedChild::new(child.id());
        assert!(adopted.try_wait().unwrap().is_none());
        adopted.kill().unwrap();
        assert!(adopted.wait().is_ok());
    }
}
