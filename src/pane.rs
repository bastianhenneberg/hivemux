//! A pane: one child process running in a pseudo terminal, plus the VT state
//! its output has produced.

use std::io::{Read, Write};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::app::AppEvent;
use crate::layout::PaneId;

const SCROLLBACK_LINES: usize = 10_000;

pub struct Pane {
    parser: Arc<Mutex<vt100::Parser>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
}

impl Pane {
    /// Spawns the user's default shell in a new pty of `rows` x `cols`.
    /// Output is fed into the VT parser on a background thread, which sends
    /// `AppEvent::PtyOutput` after every chunk and `AppEvent::PtyExited(id)` at EOF.
    pub fn spawn(id: PaneId, rows: u16, cols: u16, events: Sender<AppEvent>) -> Result<Self> {
        let rows = rows.max(1);
        let cols = cols.max(1);

        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open pty")?;

        let mut cmd = CommandBuilder::new_default_prog();
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("HIVEMUX", "1");

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn shell")?;
        // Drop our copy of the slave side, so the reader sees EOF once the
        // child exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));

        let reader_parser = Arc::clone(&parser);
        thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            reader_parser.lock().unwrap().process(&buf[..n]);
                            if events.send(AppEvent::PtyOutput).is_err() {
                                return;
                            }
                        }
                    }
                }
                let _ = events.send(AppEvent::PtyExited(id));
            })?;

        Ok(Self {
            parser,
            master: pair.master,
            writer,
            child,
            size: (rows, cols),
        })
    }

    pub fn screen(&self) -> MutexGuard<'_, vt100::Parser> {
        self.parser.lock().unwrap()
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Resizes the pty and the VT screen. A no-op when the size is unchanged.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if self.size == (rows, cols) {
            return Ok(());
        }
        self.size = (rows, cols);
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
