//! A pane: one child process running in a pseudo terminal, plus the VT state
//! its output has produced.

use std::cell::Cell;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::agent::{self, AgentState};
use crate::app::AppEvent;
use crate::layout::PaneId;

const SCROLLBACK_LINES: usize = 10_000;

pub struct Pane {
    parser: Arc<Mutex<vt100::Parser>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
    last_output: Arc<Mutex<Instant>>,
    /// The agent state the program reported with `hivemux status`.
    pub reported: Option<AgentState>,
    /// The agent's session id from its hooks, e.g. for `claude --resume`.
    pub session: Option<String>,
    /// Whether the question on the screen was answered: input came while it
    /// was shown. Cleared once no question is shown anymore.
    answered: Cell<bool>,
}

/// What to run in a new pane and where.
pub struct Spawn<'a> {
    pub id: PaneId,
    pub rows: u16,
    pub cols: u16,
    /// Start here, or in the server's directory without it.
    pub cwd: Option<&'a Path>,
    /// Run this, or the user's shell when empty.
    pub command: &'a [String],
    /// The server socket, handed to the program so `hivemux` finds it.
    pub socket: &'a Path,
}

impl Pane {
    /// Starts `spawn.command` in a new pty. Output is fed into the VT parser
    /// on a background thread, which sends `AppEvent::PtyOutput` after every
    /// chunk and `AppEvent::PtyExited(id)` at EOF.
    pub fn spawn(spawn: Spawn, events: Sender<AppEvent>) -> Result<Self> {
        let id = spawn.id;
        let rows = spawn.rows.max(1);
        let cols = spawn.cols.max(1);

        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open pty")?;

        let mut cmd = if spawn.command.is_empty() {
            CommandBuilder::new_default_prog()
        } else {
            CommandBuilder::from_argv(spawn.command.iter().map(Into::into).collect())
        };
        match spawn.cwd {
            Some(cwd) if cwd.is_dir() => cmd.cwd(cwd),
            _ => {
                if let Ok(cwd) = std::env::current_dir() {
                    cmd.cwd(cwd);
                }
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("HIVEMUX", "1");
        cmd.env("HIVEMUX_PANE", id.to_string());
        cmd.env("HIVEMUX_SOCKET", spawn.socket);

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
        let last_output = Arc::new(Mutex::new(Instant::now()));
        let reader_last_output = Arc::clone(&last_output);
        thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            reader_parser.lock().unwrap().process(&buf[..n]);
                            *reader_last_output.lock().unwrap() = Instant::now();
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
            last_output,
            reported: None,
            session: None,
            answered: Cell::new(false),
        })
    }

    /// The process in the foreground of this pane, e.g. the shell, or nvim
    /// started from it.
    fn foreground_pid(&self) -> Option<u32> {
        self.master
            .process_group_leader()
            .map(|pid| pid as u32)
            .or_else(|| self.child.process_id())
    }

    /// The command line of the foreground process.
    pub fn foreground_argv(&self) -> Vec<String> {
        let Some(pid) = self.foreground_pid() else {
            return Vec::new();
        };
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|raw| {
                raw.split(|b| *b == 0)
                    .filter(|arg| !arg.is_empty())
                    .map(|arg| String::from_utf8_lossy(arg).into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// What the agent in this pane is doing, `None` without an agent.
    pub fn agent_state(&self) -> Option<AgentState> {
        let argv = self.foreground_argv();
        let since = self.last_output.lock().unwrap().elapsed();
        let asking = agent::asks_question(self.screen().screen());
        if !asking {
            self.answered.set(false);
        }
        agent::state(&argv, self.reported, since, asking && !self.answered.get())
    }

    /// Notes input from the user or another agent: a question on the screen
    /// counts as answered.
    pub fn note_input(&self) {
        if agent::asks_question(self.screen().screen()) {
            self.answered.set(true);
        }
    }

    /// The name to show for this pane: the agent, else the foreground program.
    pub fn program(&self) -> Option<String> {
        let argv = self.foreground_argv();
        if let Some(agent) = agent::agent_name(&argv) {
            return Some(agent.to_owned());
        }
        let first = argv.first()?;
        Some(
            first
                .rsplit('/')
                .next()
                .unwrap_or(first)
                .trim_start_matches('-')
                .to_owned(),
        )
    }

    /// The working directory of the program in the foreground of this pane,
    /// e.g. the shell, or nvim started from it. Read from `/proc`, so Linux
    /// only.
    pub fn cwd(&self) -> Option<PathBuf> {
        let pid = self.foreground_pid()?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// How far the view is scrolled back into the history, 0 when the live
    /// screen is shown.
    pub fn scroll_offset(&self) -> usize {
        self.parser.lock().unwrap().screen().scrollback()
    }

    /// Scrolls the view by `lines`, positive into the history. The offset is
    /// clamped to the history there is.
    pub fn scroll(&mut self, lines: isize) {
        let mut parser = self.parser.lock().unwrap();
        let screen = parser.screen_mut();
        let offset = screen.scrollback().saturating_add_signed(lines);
        screen.set_scrollback(offset);
    }

    pub fn scroll_to(&mut self, offset: usize) {
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_scrollback(offset);
    }

    /// The number of lines in the history above the live screen.
    pub fn history_len(&self) -> usize {
        history_len(&mut self.parser.lock().unwrap())
    }

    /// The text from `start` to `end` inclusive, both as (absolute row,
    /// column), where absolute row 0 is the oldest line of the history.
    /// Lines wrapped by the terminal are joined, trailing spaces dropped.
    pub fn text(&self, start: (usize, u16), end: (usize, u16)) -> String {
        text_between(&mut self.parser.lock().unwrap(), start, end)
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

/// See `Pane::text`.
fn text_between(parser: &mut vt100::Parser, start: (usize, u16), end: (usize, u16)) -> String {
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    let history = history_len(parser);
    let screen = parser.screen_mut();
    let saved = screen.scrollback();
    let (rows, cols) = screen.size();

    let mut out = String::new();
    for abs in start.0..=end.0 {
        // Scroll so that this row is in view, then read it there.
        let offset = history.saturating_sub(abs);
        screen.set_scrollback(offset);
        let offset = screen.scrollback();
        let Some(view_row) = (abs + offset).checked_sub(history) else {
            continue;
        };
        let Ok(view_row) = u16::try_from(view_row) else {
            continue;
        };
        if view_row >= rows {
            continue;
        }
        let from = if abs == start.0 { start.1 } else { 0 };
        let to = if abs == end.0 {
            end.1.saturating_add(1).min(cols)
        } else {
            cols
        };
        let line = screen.contents_between(view_row, from, view_row, to);
        out.push_str(line.trim_end());
        if abs != end.0 && !screen.row_wrapped(view_row) {
            out.push('\n');
        }
    }
    screen.set_scrollback(saved);
    out
}

/// The history length: vt100 clamps a too large offset to it.
fn history_len(parser: &mut vt100::Parser) -> usize {
    let screen = parser.screen_mut();
    let saved = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let len = screen.scrollback();
    screen.set_scrollback(saved);
    len
}

impl Drop for Pane {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3 rows of screen, lines "line 0" .. "line 9" written, so 7 lines
    /// went into the history.
    fn parser_with_history() -> vt100::Parser {
        let mut parser = vt100::Parser::new(3, 20, 100);
        for i in 0..10 {
            parser.process(format!("line {i}").as_bytes());
            if i < 9 {
                parser.process(b"\r\n");
            }
        }
        parser
    }

    #[test]
    fn history_length_counts_scrolled_off_lines() {
        let mut parser = parser_with_history();
        assert_eq!(history_len(&mut parser), 7);
        assert_eq!(parser.screen().scrollback(), 0);
    }

    #[test]
    fn text_reaches_into_the_history() {
        let mut parser = parser_with_history();
        assert_eq!(text_between(&mut parser, (0, 0), (1, 19)), "line 0\nline 1");
        assert_eq!(
            text_between(&mut parser, (6, 5), (9, 5)),
            "6\nline 7\nline 8\nline 9"
        );
        // Reversed order works too, and the view stays where it was.
        assert_eq!(text_between(&mut parser, (2, 3), (2, 0)), "line");
        assert_eq!(parser.screen().scrollback(), 0);
    }
}
