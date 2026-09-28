//! The server's state and event loop. Pty output, client input and client
//! connections all come in over one channel, and after each batch of events
//! the screen is rendered for the attached client.

use std::collections::HashMap;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout as UiLayout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use crate::keys;
use crate::layout::{Axis, Direction, Layout, MIN_PANE_SIZE, PaneId};
use crate::pane::Pane;
use crate::protocol::{ClientMsg, ServerMsg};
use crate::render::ScreenView;

const HONEY: Color = Color::Rgb(250, 190, 0);

/// How long after a repeatable command (focus, resize) another arrow key
/// repeats it without pressing the prefix again. Same idea as tmux's
/// `repeat-time`.
const REPEAT_TIME: Duration = Duration::from_millis(600);

/// The screen size assumed until a client tells us its real one.
const DEFAULT_SCREEN: Rect = Rect::new(0, 0, 80, 24);

pub type ClientId = u64;

pub enum AppEvent {
    PtyOutput,
    PtyExited(PaneId),
    ClientConnected(UnixStream),
    ClientMsg(ClientId, ClientMsg),
    ClientGone(ClientId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    /// The prefix was pressed, the next key is a command.
    Prefix,
    /// A repeatable command just ran, arrow keys repeat it until the deadline.
    Repeat(Instant),
    /// A destructive action waits for the user to confirm it with `y`.
    Confirm(Action),
}

/// Actions that destroy something and are therefore confirmed first.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    ClosePane(PaneId),
    KillServer,
}

/// The attached client. Rendering goes through a ratatui terminal whose
/// output is sent to the client instead of being written to a tty.
struct Client {
    id: ClientId,
    stream: UnixStream,
    terminal: Terminal<CrosstermBackend<FrameSink>>,
}

impl Client {
    /// A terminal of exactly `screen`'s size. It never asks a tty for its
    /// size, the server has none, the client reports it instead.
    fn terminal_for(
        stream: &UnixStream,
        screen: Rect,
    ) -> io::Result<Terminal<CrosstermBackend<FrameSink>>> {
        let sink = FrameSink {
            stream: stream.try_clone()?,
            buf: Vec::new(),
        };
        Terminal::with_options(
            CrosstermBackend::new(sink),
            TerminalOptions {
                viewport: Viewport::Fixed(screen),
            },
        )
    }

    fn send(&mut self, msg: &ServerMsg) -> io::Result<()> {
        msg.write_to(&mut self.stream)
    }
}

/// Collects what ratatui writes and sends it to the client as one
/// `ServerMsg::Output` frame per flush.
struct FrameSink {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl Write for FrameSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        ServerMsg::Output(std::mem::take(&mut self.buf)).write_to(&mut self.stream)
    }
}

pub struct App {
    panes: HashMap<PaneId, Pane>,
    layout: Layout,
    focus: PaneId,
    next_id: PaneId,
    /// The attached client's screen.
    screen: Rect,
    /// Where the panes are drawn, i.e. the screen minus the status bar.
    body: Rect,
    client: Option<Client>,
    next_client_id: ClientId,
    events: Sender<AppEvent>,
    title: String,
    mode: Mode,
    quit: bool,
}

impl App {
    /// Starts with one pane and runs until the last pane is closed or the
    /// server is told to shut down.
    pub fn run(events: Sender<AppEvent>, rx: &Receiver<AppEvent>) -> Result<()> {
        let body = body_area(DEFAULT_SCREEN);
        let inner = pane_inner(body);
        let first = Pane::spawn(0, inner.height, inner.width, events.clone())?;

        let mut app = App {
            panes: HashMap::from([(0, first)]),
            layout: Layout::new(0),
            focus: 0,
            next_id: 1,
            screen: DEFAULT_SCREEN,
            body,
            client: None,
            next_client_id: 0,
            events,
            title: shell_name(),
            mode: Mode::Normal,
            quit: false,
        };
        app.event_loop(rx);
        if let Some(mut client) = app.client.take() {
            let _ = client.send(&ServerMsg::Exited);
        }
        Ok(())
    }

    fn event_loop(&mut self, rx: &Receiver<AppEvent>) {
        while !self.quit {
            self.body = body_area(self.screen);
            self.sync_sizes();
            self.render();

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
            self.handle(event);
            while let Ok(event) = rx.try_recv() {
                self.handle(event);
            }
        }
    }

    /// Gives every pty the size of the area inside its pane's border.
    fn sync_sizes(&mut self) {
        for (id, rect) in self.layout.rects(self.body) {
            if let Some(pane) = self.panes.get_mut(&id) {
                let inner = pane_inner(rect);
                if let Err(e) = pane.resize(inner.height, inner.width) {
                    eprintln!("failed to resize pane {id}: {e:#}");
                }
            }
        }
    }

    /// Draws the screen for the attached client. A client that cannot be
    /// written to anymore is dropped.
    fn render(&mut self) {
        let Some(mut client) = self.client.take() else {
            return;
        };
        if client.terminal.draw(|frame| self.draw(frame)).is_ok() {
            self.client = Some(client);
        }
    }

    fn handle(&mut self, event: AppEvent) {
        let result = match event {
            AppEvent::PtyOutput => Ok(()),
            AppEvent::PtyExited(id) => {
                self.close(id);
                Ok(())
            }
            AppEvent::ClientConnected(stream) => self.attach(stream),
            AppEvent::ClientMsg(id, msg) if self.client.as_ref().is_some_and(|c| c.id == id) => {
                self.handle_client_msg(msg)
            }
            AppEvent::ClientMsg(_, ClientMsg::KillServer) => {
                self.quit = true;
                Ok(())
            }
            AppEvent::ClientMsg(..) => Ok(()),
            AppEvent::ClientGone(id) => {
                if self.client.as_ref().is_some_and(|c| c.id == id) {
                    self.client = None;
                }
                Ok(())
            }
        };
        if let Err(e) = result {
            eprintln!("{e:#}");
        }
    }

    /// Makes `stream` the attached client, detaching the previous one.
    fn attach(&mut self, stream: UnixStream) -> Result<()> {
        self.detach();

        let id = self.next_client_id;
        self.next_client_id += 1;

        let mut reader = stream.try_clone()?;
        let events = self.events.clone();
        thread::Builder::new()
            .name(format!("client-{id}"))
            .spawn(move || {
                while let Ok(Some(msg)) = ClientMsg::read_from(&mut reader) {
                    if events.send(AppEvent::ClientMsg(id, msg)).is_err() {
                        return;
                    }
                }
                let _ = events.send(AppEvent::ClientGone(id));
            })?;

        let terminal = Client::terminal_for(&stream, self.screen)?;
        self.client = Some(Client {
            id,
            stream,
            terminal,
        });
        self.mode = Mode::Normal;
        Ok(())
    }

    fn detach(&mut self) {
        if let Some(mut client) = self.client.take() {
            let _ = client.send(&ServerMsg::Detached);
        }
    }

    fn handle_client_msg(&mut self, msg: ClientMsg) -> Result<()> {
        match msg {
            ClientMsg::Event(Event::Key(key)) => self.handle_key(key),
            ClientMsg::Event(Event::Paste(text)) => self.paste(&text),
            ClientMsg::Event(Event::Resize(cols, rows)) => self.resize_screen(cols, rows),
            ClientMsg::Event(_) => Ok(()),
            ClientMsg::KillServer => {
                self.quit = true;
                Ok(())
            }
        }
    }

    /// Adopts the client's new screen size. The terminal is rebuilt rather
    /// than resized, since resizing makes ratatui ask the tty for its size.
    fn resize_screen(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.screen = Rect::new(0, 0, cols, rows);
        if let Some(client) = &mut self.client {
            client.send(&ServerMsg::Output(b"\x1b[2J".to_vec()))?;
            client.terminal = Client::terminal_for(&client.stream, self.screen)?;
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
            Mode::Confirm(action) => {
                self.mode = Mode::Normal;
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    self.perform(action);
                }
                Ok(())
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
            // Like herdr, q detaches. Stopping the server and its shells
            // takes `hivemux kill-server`, so no key can do it by accident.
            KeyCode::Char('q' | 'd') => self.detach(),
            KeyCode::Char('%') => self.split(Axis::Row),
            KeyCode::Char('"') => self.split(Axis::Column),
            KeyCode::Char('x') => self.mode = Mode::Confirm(Action::ClosePane(self.focus)),
            KeyCode::Char('Q') => self.mode = Mode::Confirm(Action::KillServer),
            KeyCode::Char('o') => self.cycle_focus(),
            _ => {}
        }
        Ok(())
    }

    fn perform(&mut self, action: Action) {
        match action {
            Action::ClosePane(id) => self.close(id),
            Action::KillServer => self.quit = true,
        }
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
            if focused
                && !matches!(self.mode, Mode::Confirm(_))
                && let Some(position) = cursor
            {
                frame.set_cursor_position(position);
            }
        }

        frame.render_widget(self.status_line(), status_area);

        if let Mode::Confirm(action) = self.mode {
            self.draw_confirm(frame, body, action);
        }
    }

    /// A warning box in the middle of the screen, asking to confirm `action`.
    fn draw_confirm(&self, frame: &mut Frame, area: Rect, action: Action) {
        let shells = self.panes.len();
        let (title, lines) = match action {
            Action::ClosePane(id) => (
                " Close pane ",
                vec![Line::from(format!(
                    "Close pane {id} and end the program running in it?"
                ))],
            ),
            Action::KillServer => (
                " Quit hivemux ",
                vec![
                    Line::from(format!(
                        "End the session and all {shells} {} in it?",
                        if shells == 1 { "shell" } else { "shells" }
                    )),
                    Line::from(Span::styled(
                        "To keep them running, detach with ^B q instead.",
                        Style::new().add_modifier(Modifier::DIM),
                    )),
                ],
            ),
        };

        let warning = Style::new()
            .fg(Color::LightRed)
            .add_modifier(Modifier::BOLD);
        let mut text = lines;
        text.push(Line::default());
        text.push(Line::from(vec![
            Span::styled(
                " y ",
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::LightRed)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" yes    "),
            Span::styled("any other key", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" cancel"),
        ]));

        let width = text.iter().map(Line::width).max().unwrap_or(0) as u16 + 6;
        let height = text.len() as u16 + 2;
        let popup = centered(area, width, height);

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(warning)
            .title(Span::styled(format!(" ⚠{title}"), warning))
            .padding(Padding::horizontal(2));
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(text).block(block), popup);
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
                    "  % split │  \" split ─  x close  o next  ←↑↓→ focus  ^←↑↓→ resize  q/d detach  Q quit",
                    Style::new().fg(HONEY),
                ));
            }
            Mode::Confirm(_) => {
                spans.push(Span::styled(
                    " CONFIRM ",
                    Style::new()
                        .fg(Color::Black)
                        .bg(Color::LightRed)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    "  y yes · any other key cancels",
                    Style::new().fg(Color::LightRed),
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
                        "{} {} · ^B % \" split · ^B q detach",
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

/// A `width` x `height` rect in the middle of `area`, shrunk to fit.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
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
