//! The event loop: terminal input and pty output come in over one channel,
//! the screen is redrawn after each batch of events.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout as UiLayout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};

use crate::keys;
use crate::layout::{Axis, Direction, Layout, MIN_PANE_SIZE, PaneId};
use crate::pane::Pane;
use crate::render::ScreenView;

const HONEY: Color = Color::Rgb(250, 190, 0);

/// How long after a repeatable command (focus, resize) another arrow key
/// repeats it without pressing the prefix again. Same idea as tmux's
/// `repeat-time`.
const REPEAT_TIME: Duration = Duration::from_millis(600);

pub enum AppEvent {
    Term(Event),
    PtyOutput,
    PtyExited(PaneId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    /// The prefix was pressed, the next key is a command.
    Prefix,
    /// A repeatable command just ran, arrow keys repeat it until the deadline.
    Repeat(Instant),
}

pub struct App {
    panes: HashMap<PaneId, Pane>,
    layout: Layout,
    focus: PaneId,
    next_id: PaneId,
    /// Where the panes are drawn, i.e. the screen minus the status bar.
    body: Rect,
    events: Sender<AppEvent>,
    title: String,
    mode: Mode,
    quit: bool,
}

impl App {
    pub fn run(terminal: &mut DefaultTerminal) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        spawn_input_thread(tx.clone())?;

        let size = terminal.size()?;
        let body = body_area(Rect::new(0, 0, size.width, size.height));
        let inner = pane_inner(body);
        let first = Pane::spawn(0, inner.height, inner.width, tx.clone())?;

        let mut app = App {
            panes: HashMap::from([(0, first)]),
            layout: Layout::new(0),
            focus: 0,
            next_id: 1,
            body,
            events: tx,
            title: shell_name(),
            mode: Mode::Normal,
            quit: false,
        };
        app.event_loop(terminal, &rx)
    }

    fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        rx: &Receiver<AppEvent>,
    ) -> Result<()> {
        while !self.quit {
            let size = terminal.size()?;
            self.body = body_area(Rect::new(0, 0, size.width, size.height));
            self.sync_sizes()?;
            terminal.draw(|frame| self.draw(frame))?;

            // Block for the next event, then take everything else that is
            // already queued, so a burst of output costs one redraw.
            let event = match self.mode {
                Mode::Repeat(until) => {
                    match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                        Ok(event) => event,
                        Err(RecvTimeoutError::Timeout) => {
                            self.mode = Mode::Normal;
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                _ => match rx.recv() {
                    Ok(event) => event,
                    Err(_) => break,
                },
            };
            self.handle(event)?;
            while let Ok(event) = rx.try_recv() {
                self.handle(event)?;
            }
        }
        Ok(())
    }

    /// Gives every pty the size of the area inside its pane's border.
    fn sync_sizes(&mut self) -> Result<()> {
        for (id, rect) in self.layout.rects(self.body) {
            if let Some(pane) = self.panes.get_mut(&id) {
                let inner = pane_inner(rect);
                pane.resize(inner.height, inner.width)?;
            }
        }
        Ok(())
    }

    fn handle(&mut self, event: AppEvent) -> Result<()> {
        match event {
            AppEvent::Term(Event::Key(key)) => self.handle_key(key)?,
            AppEvent::Term(Event::Paste(text)) => self.paste(&text)?,
            AppEvent::Term(_) | AppEvent::PtyOutput => {}
            AppEvent::PtyExited(id) => self.close(id),
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind == KeyEventKind::Release {
            return Ok(());
        }

        match self.mode {
            Mode::Prefix => {
                self.mode = Mode::Normal;
                self.command(key)
            }
            Mode::Repeat(until) if Instant::now() < until && arrow(key.code).is_some() => {
                self.command(key)
            }
            _ => {
                self.mode = Mode::Normal;
                if is_prefix(key) {
                    self.mode = Mode::Prefix;
                    return Ok(());
                }
                let Some(pane) = self.panes.get_mut(&self.focus) else {
                    return Ok(());
                };
                let app_cursor = pane.screen().screen().application_cursor();
                if let Some(bytes) = keys::encode(key, app_cursor) {
                    pane.write(&bytes)?;
                }
                Ok(())
            }
        }
    }

    /// Runs the command bound to `key` after the prefix.
    fn command(&mut self, key: KeyEvent) -> Result<()> {
        if is_prefix(key) {
            // Prefix twice sends the prefix key itself to the pane.
            if let Some(pane) = self.panes.get_mut(&self.focus) {
                pane.write(&[0x02])?;
            }
            return Ok(());
        }

        if let Some(dir) = arrow(key.code) {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                self.layout.resize(self.focus, dir, 1, self.body);
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                self.layout.resize(self.focus, dir, 5, self.body);
            } else if let Some(next) = self.layout.neighbor(self.focus, dir, self.body) {
                self.focus = next;
            }
            self.mode = Mode::Repeat(Instant::now() + REPEAT_TIME);
            return Ok(());
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('%') => self.split(Axis::Row),
            KeyCode::Char('"') => self.split(Axis::Column),
            KeyCode::Char('x') => self.close(self.focus),
            KeyCode::Char('o') => self.cycle_focus(),
            _ => {}
        }
        Ok(())
    }

    /// Splits the focused pane and focuses the new one. Does nothing if the
    /// pane is too small to be split.
    fn split(&mut self, axis: Axis) {
        let Some((_, rect)) = self
            .layout
            .rects(self.body)
            .into_iter()
            .find(|(id, _)| *id == self.focus)
        else {
            return;
        };
        let room = match axis {
            Axis::Row => rect.width,
            Axis::Column => rect.height,
        };
        if room < 2 * MIN_PANE_SIZE {
            return;
        }

        let id = self.next_id;
        self.next_id += 1;
        self.layout.split(self.focus, axis, id);

        let rect = self
            .layout
            .rects(self.body)
            .into_iter()
            .find_map(|(pane, rect)| (pane == id).then_some(rect))
            .unwrap_or(rect);
        let inner = pane_inner(rect);
        match Pane::spawn(id, inner.height, inner.width, self.events.clone()) {
            Ok(pane) => {
                self.panes.insert(id, pane);
                self.focus = id;
            }
            Err(_) => {
                self.layout.remove(id);
            }
        }
    }

    /// Removes pane `id`, killing its process if it still runs. Quits when
    /// the last pane is gone.
    fn close(&mut self, id: PaneId) {
        if self.panes.remove(&id).is_none() {
            return;
        }
        let next = self.layout.remove(id);
        if self.focus == id
            && let Some(next) = next
        {
            self.focus = next;
        }
        if self.layout.is_empty() {
            self.quit = true;
        }
    }

    fn cycle_focus(&mut self) {
        let panes = self.layout.panes();
        if let Some(i) = panes.iter().position(|&id| id == self.focus) {
            self.focus = panes[(i + 1) % panes.len()];
        }
    }

    fn paste(&mut self, text: &str) -> Result<()> {
        let Some(pane) = self.panes.get_mut(&self.focus) else {
            return Ok(());
        };
        let bracketed = pane.screen().screen().bracketed_paste();
        if bracketed {
            pane.write(b"\x1b[200~")?;
            pane.write(text.as_bytes())?;
            pane.write(b"\x1b[201~")
        } else {
            pane.write(text.replace('\n', "\r").as_bytes())
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let [body, status_area] = split_screen(frame.area());

        for (id, rect) in self.layout.rects(body) {
            let Some(pane) = self.panes.get(&id) else {
                continue;
            };
            let focused = id == self.focus;
            let border = if focused {
                Style::new().fg(HONEY)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(border)
                .title(format!(" {id} {} ", self.title));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);

            let parser = pane.screen();
            let view = ScreenView::new(parser.screen());
            let cursor = view.cursor(inner);
            frame.render_widget(view, inner);
            if focused && let Some(position) = cursor {
                frame.set_cursor_position(position);
            }
        }

        frame.render_widget(self.status_line(), status_area);
    }

    fn status_line(&self) -> Line<'static> {
        let badge = Style::new()
            .fg(Color::Black)
            .bg(HONEY)
            .add_modifier(Modifier::BOLD);
        let hint = Style::new().add_modifier(Modifier::DIM);
        let mut spans = vec![Span::styled(" ⬢ hivemux ", badge), Span::raw(" ")];
        match self.mode {
            Mode::Prefix => {
                spans.push(Span::styled(" PREFIX ", badge));
                spans.push(Span::styled(
                    "  % split │  \" split ─  x close  o next  ←↑↓→ focus  ^←↑↓→ resize  q quit",
                    Style::new().fg(HONEY),
                ));
            }
            Mode::Repeat(_) => {
                spans.push(Span::styled(" REPEAT ", badge));
                spans.push(Span::styled(
                    "  ←↑↓→ focus  ^←↑↓→ resize",
                    Style::new().fg(HONEY),
                ));
            }
            Mode::Normal => {
                spans.push(Span::styled(
                    format!(
                        "{} {} · ^B % \" split · ^B q quit",
                        self.panes.len(),
                        if self.panes.len() == 1 {
                            "pane"
                        } else {
                            "panes"
                        }
                    ),
                    hint,
                ));
            }
        }
        Line::from(spans)
    }
}

fn is_prefix(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn arrow(code: KeyCode) -> Option<Direction> {
    match code {
        KeyCode::Left => Some(Direction::Left),
        KeyCode::Right => Some(Direction::Right),
        KeyCode::Up => Some(Direction::Up),
        KeyCode::Down => Some(Direction::Down),
        _ => None,
    }
}

/// Panes on top, one line of status bar at the bottom.
fn split_screen(area: Rect) -> [Rect; 2] {
    UiLayout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area)
}

fn body_area(screen: Rect) -> Rect {
    split_screen(screen)[0]
}

/// The area inside a pane's border, i.e. the size of its pty.
fn pane_inner(rect: Rect) -> Rect {
    Block::bordered().inner(rect)
}

fn shell_name() -> String {
    std::env::var("SHELL")
        .ok()
        .and_then(|shell| shell.rsplit('/').next().map(str::to_owned))
        .unwrap_or_else(|| "shell".into())
}

fn spawn_input_thread(tx: Sender<AppEvent>) -> Result<()> {
    thread::Builder::new().name("input".into()).spawn(move || {
        while let Ok(event) = event::read() {
            if tx.send(AppEvent::Term(event)).is_err() {
                break;
            }
        }
    })?;
    Ok(())
}
