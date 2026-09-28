//! The server's state and event loop. Pty output, client input and client
//! connections all come in over one channel, and after each batch of events
//! the screen is rendered for the attached client.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use crate::bindings::{self, Command, Menu};
use crate::config::{Bars, Config, Placement, SETTINGS};
use crate::keys;
use crate::layout::{Axis, MIN_PANE_SIZE, PaneId};
use crate::menu::{self, HONEY};
use crate::pane::Pane;
use crate::protocol::{ClientMsg, ServerMsg};
use crate::render::ScreenView;
use crate::workspace::Workspace;
use crate::{clipboard, mouse};

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
    /// The prefix was pressed, the next key is a command in this menu.
    Prefix(Menu),
    /// A repeatable command just ran from this menu, its keys repeat without
    /// the prefix until the deadline.
    Repeat(Instant, Menu),
    /// A destructive action waits for the user to confirm it with `y`.
    Confirm(Action),
    /// The key reference is shown, the next key closes it.
    Help,
    /// The quit menu is open with the given entry selected.
    Menu(usize),
    /// The settings menu is open with the given setting selected.
    Settings(usize),
    /// Scrolling and selecting with the keyboard, see `App::copy`.
    Copy,
}

/// A mouse drag in progress.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    /// Moving a floating pane by its title bar, grabbed `dx` cells from its
    /// left edge.
    Move { id: PaneId, dx: u16 },
    /// Resizing a floating pane by its bottom right corner.
    Resize { id: PaneId },
    /// Selecting text.
    Select,
}

/// Selected text in a pane, from `anchor` to `head`, as (absolute row,
/// column) where row 0 is the oldest line of the pane's history.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Selection {
    pane: PaneId,
    anchor: (usize, u16),
    head: (usize, u16),
}

/// The copy mode cursor, in the same coordinates as a selection.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CopyCursor {
    pane: PaneId,
    pos: (usize, u16),
}

/// The entries of the quit menu, in order.
const MENU: [(char, MenuEntry); 3] = [
    ('d', MenuEntry::Detach),
    ('x', MenuEntry::ClosePane),
    ('Q', MenuEntry::EndSession),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuEntry {
    Detach,
    ClosePane,
    EndSession,
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
    /// The active workspace.
    ws: Workspace,
    /// The number of the active workspace.
    workspace: u8,
    /// The other workspaces. Their panes keep running, they are just not
    /// drawn.
    hidden: BTreeMap<u8, Workspace>,
    next_id: PaneId,
    /// The attached client's screen.
    screen: Rect,
    /// Where the panes are drawn, i.e. the screen minus the status bar.
    body: Rect,
    client: Option<Client>,
    next_client_id: ClientId,
    events: Sender<AppEvent>,
    title: String,
    config: Config,
    /// Shown in the settings menu: where the config was saved, or why it
    /// could not be loaded or saved.
    config_note: Option<String>,
    mode: Mode,
    drag: Option<Drag>,
    selection: Option<Selection>,
    copy: Option<CopyCursor>,
    /// A short message for the control bar, e.g. what was copied. Cleared by
    /// the next key.
    flash: Option<String>,
    quit: bool,
}

impl App {
    /// Starts with one pane and runs until the last pane is closed or the
    /// server is told to shut down.
    pub fn run(events: Sender<AppEvent>, rx: &Receiver<AppEvent>) -> Result<()> {
        let (config, config_error) = Config::load();
        if let Some(e) = &config_error {
            eprintln!("config: {e}");
        }
        let body = screen_layout(DEFAULT_SCREEN, &config.bars).body;
        let inner = pane_inner(body);
        let first = Pane::spawn(0, inner.height, inner.width, None, events.clone())?;

        let mut app = App {
            panes: HashMap::from([(0, first)]),
            ws: Workspace::new(0),
            workspace: 1,
            hidden: BTreeMap::new(),
            next_id: 1,
            screen: DEFAULT_SCREEN,
            body,
            client: None,
            next_client_id: 0,
            events,
            title: shell_name(),
            config,
            config_note: config_error.map(|e| format!("config ignored: {e}")),
            mode: Mode::Normal,
            drag: None,
            selection: None,
            copy: None,
            flash: None,
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
            self.body = screen_layout(self.screen, &self.config.bars).body;
            self.sync_sizes();
            self.render();

            // Block for the next event, then take everything else that is
            // already queued, so a burst of output costs one redraw.
            let event = match self.mode {
                Mode::Repeat(until, _) => {
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
        for (id, rect) in self.ws.rects(self.body) {
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
            ClientMsg::Event(Event::Mouse(event)) => self.mouse(event),
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
            Mode::Prefix(menu) => {
                self.mode = Mode::Normal;
                self.command(menu, key)
            }
            Mode::Help => {
                self.mode = Mode::Normal;
                Ok(())
            }
            Mode::Menu(selected) => {
                self.menu_key(key, selected);
                Ok(())
            }
            Mode::Settings(selected) => {
                self.settings_key(key, selected);
                Ok(())
            }
            Mode::Copy => {
                self.copy_key(key);
                Ok(())
            }
            Mode::Confirm(action) => {
                self.mode = Mode::Normal;
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    self.perform(action);
                }
                Ok(())
            }
            Mode::Repeat(until, menu)
                if Instant::now() < until
                    && bindings::lookup(menu, key).is_some_and(Command::repeatable) =>
            {
                self.command(menu, key)
            }
            _ => {
                self.mode = Mode::Normal;
                if is_prefix(key) {
                    self.mode = Mode::Prefix(Menu::Root);
                    return Ok(());
                }
                self.flash = None;
                self.selection = None;
                let Some(pane) = self.panes.get_mut(&self.ws.focus) else {
                    return Ok(());
                };
                // Typing brings a scrolled back pane to its live screen.
                if pane.scroll_offset() > 0 {
                    pane.scroll_to(0);
                }
                let app_cursor = pane.screen().screen().application_cursor();
                if let Some(bytes) = keys::encode(key, app_cursor) {
                    pane.write(&bytes)?;
                }
                Ok(())
            }
        }
    }

    /// Runs the command bound to `key` after the prefix. Unbound keys just
    /// end the prefix.
    fn command(&mut self, menu: Menu, key: KeyEvent) -> Result<()> {
        let Some(command) = bindings::lookup(menu, key) else {
            return Ok(());
        };
        match command {
            Command::SplitRow => self.split(Axis::Row),
            Command::SplitColumn => self.split(Axis::Column),
            Command::ClosePane => self.mode = Mode::Confirm(Action::ClosePane(self.ws.focus)),
            Command::NextPane => self.ws.cycle_focus(),
            Command::Focus(dir) => self.ws.focus_direction(dir, self.body),
            Command::Resize(dir, cells) => {
                if !self.ws.resize_float(dir, cells, self.body) {
                    self.ws.layout.resize(self.ws.focus, dir, cells, self.body);
                }
            }
            Command::Move(dir, cells) => {
                self.ws.move_float(dir, cells, self.body);
            }
            Command::ToggleFloat => self.ws.toggle_float(self.body),
            Command::NewFloat => self.new_float(),
            Command::NextFloat => self.ws.next_float(),
            Command::Open(menu) => self.mode = Mode::Prefix(menu),
            Command::Back => self.mode = Mode::Prefix(Menu::Root),
            Command::CopyMode => self.enter_copy(),
            Command::ScrollBack => {
                self.enter_copy();
                let page = self.focused_inner().map_or(1, |r| r.height as isize);
                self.copy_scroll(page);
            }
            Command::Detach => self.detach(),
            Command::Quit => self.mode = Mode::Confirm(Action::KillServer),
            Command::Help => self.mode = Mode::Help,
            Command::SessionMenu => self.mode = Mode::Menu(0),
            Command::Settings => self.mode = Mode::Settings(0),
            Command::Workspace(n) => self.switch_workspace(n),
            Command::NewWorkspace => {
                if let Some(n) = (1..=9).find(|n| !self.workspace_exists(*n)) {
                    self.switch_workspace(n);
                }
            }
            Command::NextWorkspace => self.step_workspace(true),
            Command::PrevWorkspace => self.step_workspace(false),
            // Prefix twice sends the prefix key itself to the pane.
            Command::SendPrefix => {
                if let Some(pane) = self.panes.get_mut(&self.ws.focus) {
                    pane.write(&[0x02])?;
                }
            }
            Command::Cancel => {}
        }
        if command.repeatable() {
            self.mode = Mode::Repeat(Instant::now() + REPEAT_TIME, menu);
        }
        Ok(())
    }

    /// Moves through the quit menu with arrows or j/k and picks an entry
    /// with Enter or its letter. Choosing an entry there is already a
    /// deliberate decision, so it is not confirmed again.
    fn menu_key(&mut self, key: KeyEvent, selected: usize) {
        let pick = match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
                return;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode = Mode::Menu((selected + MENU.len() - 1) % MENU.len());
                return;
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.mode = Mode::Menu((selected + 1) % MENU.len());
                return;
            }
            KeyCode::Enter => Some(MENU[selected].1),
            KeyCode::Char(c) => MENU.iter().find(|(k, _)| *k == c).map(|&(_, e)| e),
            _ => None,
        };
        let Some(entry) = pick else { return };
        self.mode = Mode::Normal;
        match entry {
            MenuEntry::Detach => self.detach(),
            MenuEntry::ClosePane => self.close(self.ws.focus),
            MenuEntry::EndSession => self.quit = true,
        }
    }

    /// Moves through the settings and changes the selected one. Every
    /// change applies right away and is saved to the config file.
    fn settings_key(&mut self, key: KeyEvent, selected: usize) {
        let count = SETTINGS.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | ',') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode = Mode::Settings((selected + count - 1) % count);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.mode = Mode::Settings((selected + 1) % count);
            }
            KeyCode::Enter | KeyCode::Char(' ' | 'h' | 'l') | KeyCode::Left | KeyCode::Right => {
                (SETTINGS[selected].cycle)(&mut self.config);
                self.config_note = Some(match self.config.save() {
                    Ok(path) => format!("saved to {}", tilde(&path)),
                    Err(e) => format!("not saved: {e:#}"),
                });
            }
            _ => {}
        }
    }

    /// The area inside the focused pane's border.
    fn focused_inner(&self) -> Option<Rect> {
        self.inner_of(self.ws.focus)
    }

    fn inner_of(&self, id: PaneId) -> Option<Rect> {
        self.ws
            .rects(self.body)
            .into_iter()
            .find_map(|(pane, rect)| (pane == id).then(|| pane_inner(rect)))
    }

    /// The absolute position of a cell of pane `id` at `row`, `col` in its
    /// current view.
    fn to_abs(&self, id: PaneId, row: u16, col: u16) -> (usize, u16) {
        let Some(pane) = self.panes.get(&id) else {
            return (0, col);
        };
        let top = pane.history_len() - pane.scroll_offset();
        (top + usize::from(row), col)
    }

    /// The view row of absolute row `abs` in pane `id`, outside the view
    /// when negative or past the bottom.
    fn to_view(&self, id: PaneId, abs: usize) -> i64 {
        let Some(pane) = self.panes.get(&id) else {
            return -1;
        };
        let top = pane.history_len() - pane.scroll_offset();
        abs as i64 - top as i64
    }

    /// Copies the selection to the client's clipboard.
    fn copy_selection(&mut self) {
        let Some(sel) = self.selection else { return };
        let Some(pane) = self.panes.get(&sel.pane) else {
            return;
        };
        let text = pane.text(sel.anchor, sel.head);
        if text.is_empty() {
            return;
        }
        let chars = text.chars().count();
        if let Some(client) = &mut self.client {
            let _ = client.send(&ServerMsg::Output(clipboard::osc52(&text)));
        }
        self.flash = Some(format!(
            "copied {chars} {}",
            if chars == 1 {
                "character"
            } else {
                "characters"
            }
        ));
    }

    fn enter_copy(&mut self) {
        let id = self.ws.focus;
        let Some(pane) = self.panes.get(&id) else {
            return;
        };
        let (row, col) = pane.screen().screen().cursor_position();
        let pos = self.to_abs(id, row, col);
        self.copy = Some(CopyCursor { pane: id, pos });
        self.selection = None;
        self.mode = Mode::Copy;
    }

    fn leave_copy(&mut self) {
        if let Some(copy) = self.copy.take()
            && let Some(pane) = self.panes.get_mut(&copy.pane)
        {
            pane.scroll_to(0);
        }
        self.mode = Mode::Normal;
    }

    /// Moves the copy cursor by `rows` and `cols`, scrolling to keep it in
    /// view and dragging the selection's head along.
    fn copy_move(&mut self, rows: isize, cols: isize) {
        let Some(mut copy) = self.copy else { return };
        let Some(inner) = self.inner_of(copy.pane) else {
            return;
        };
        let Some(pane) = self.panes.get(&copy.pane) else {
            return;
        };
        let last_row = pane.history_len() + usize::from(inner.height).saturating_sub(1);
        copy.pos.0 = copy.pos.0.saturating_add_signed(rows).min(last_row);
        let col = (copy.pos.1 as isize + cols).clamp(0, inner.width.saturating_sub(1) as isize);
        copy.pos.1 = col as u16;
        self.copy = Some(copy);
        self.copy_show(copy);
    }

    /// Scrolls the copy mode view by `lines`, positive into the history,
    /// and moves the cursor along, like Ctrl-U and Ctrl-D in vim.
    fn copy_scroll(&mut self, lines: isize) {
        let Some(mut copy) = self.copy else { return };
        let Some(pane) = self.panes.get_mut(&copy.pane) else {
            return;
        };
        let before = pane.scroll_offset();
        pane.scroll(lines);
        let moved = pane.scroll_offset() as isize - before as isize;
        copy.pos.0 = copy.pos.0.saturating_add_signed(-moved);
        self.copy = Some(copy);
        if let Some(sel) = &mut self.selection {
            sel.head = copy.pos;
        }
    }

    /// Scrolls pane so that the copy cursor is in view.
    fn copy_show(&mut self, copy: CopyCursor) {
        let Some(inner) = self.inner_of(copy.pane) else {
            return;
        };
        let view = self.to_view(copy.pane, copy.pos.0);
        let Some(pane) = self.panes.get_mut(&copy.pane) else {
            return;
        };
        if view < 0 {
            pane.scroll(-view as isize);
        } else if view >= i64::from(inner.height) {
            pane.scroll(-((view - i64::from(inner.height) + 1) as isize));
        }
        if let Some(sel) = &mut self.selection {
            sel.head = copy.pos;
        }
    }

    /// Copy mode: vim-like keys move a cursor through the pane and its
    /// history, `v` starts a selection, `y` copies it.
    fn copy_key(&mut self, key: KeyEvent) {
        let Some(copy) = self.copy else {
            self.mode = Mode::Normal;
            return;
        };
        let half = self
            .inner_of(copy.pane)
            .map_or(1, |r| (r.height / 2).max(1) as isize);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.leave_copy(),
            KeyCode::Char('u') if ctrl => self.copy_scroll(half),
            KeyCode::Char('d') if ctrl => self.copy_scroll(-half),
            KeyCode::Char('b') if ctrl => self.copy_scroll(2 * half),
            KeyCode::Char('f') if ctrl => self.copy_scroll(-2 * half),
            KeyCode::PageUp => self.copy_scroll(2 * half),
            KeyCode::PageDown => self.copy_scroll(-2 * half),
            KeyCode::Char('k') | KeyCode::Up => self.copy_move(-1, 0),
            KeyCode::Char('j') | KeyCode::Down => self.copy_move(1, 0),
            KeyCode::Char('h') | KeyCode::Left => self.copy_move(0, -1),
            KeyCode::Char('l') | KeyCode::Right => self.copy_move(0, 1),
            KeyCode::Char('0') | KeyCode::Home => self.copy_move(0, -10_000),
            KeyCode::Char('$') | KeyCode::End => self.copy_move(0, 10_000),
            KeyCode::Char('g') => self.copy_move(-(isize::MAX / 2), 0),
            KeyCode::Char('G') => self.copy_move(isize::MAX / 2, 0),
            KeyCode::Char('v' | ' ') => {
                self.selection = match self.selection {
                    Some(_) => None,
                    None => Some(Selection {
                        pane: copy.pane,
                        anchor: copy.pos,
                        head: copy.pos,
                    }),
                };
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                self.copy_selection();
                self.selection = None;
                self.leave_copy();
            }
            _ => {}
        }
    }

    /// Mouse: click focuses, dragging a floating pane's title bar moves it
    /// and its bottom right corner resizes it, dragging over text selects and
    /// copies it, the wheel scrolls back. Programs that asked for the mouse
    /// get the events instead, unless Shift is held.
    fn mouse(&mut self, event: MouseEvent) -> Result<()> {
        if !matches!(self.mode, Mode::Normal | Mode::Repeat(..) | Mode::Copy) {
            return Ok(());
        }
        let pos = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = None;
                self.flash = None;
                if let Some(n) = self.tab_at(pos) {
                    self.switch_workspace(n);
                    return Ok(());
                }
                let Some((id, rect)) = self.ws.pane_at(pos, self.body) else {
                    return Ok(());
                };
                if self.copy.is_some_and(|c| c.pane != id) {
                    self.leave_copy();
                }
                self.ws.focus(id);
                if self.ws.is_floating(id) {
                    if pos.y == rect.y {
                        self.drag = Some(Drag::Move {
                            id,
                            dx: pos.x - rect.x,
                        });
                        return Ok(());
                    }
                    if pos.x == rect.right() - 1 && pos.y == rect.bottom() - 1 {
                        self.drag = Some(Drag::Resize { id });
                        return Ok(());
                    }
                }
                let inner = pane_inner(rect);
                if !inner.contains(pos) || self.forward_mouse(id, event, inner)? {
                    return Ok(());
                }
                let at = self.to_abs(id, pos.y - inner.y, pos.x - inner.x);
                self.selection = Some(Selection {
                    pane: id,
                    anchor: at,
                    head: at,
                });
                self.drag = Some(Drag::Select);
            }
            MouseEventKind::Drag(MouseButton::Left) => match self.drag {
                Some(Drag::Move { id, dx }) => {
                    if let Some((_, rect)) =
                        self.ws.rects(self.body).into_iter().find(|(p, _)| *p == id)
                    {
                        let moved = Rect {
                            x: pos.x.saturating_sub(dx),
                            y: pos.y,
                            ..rect
                        };
                        self.ws.set_float_rect(id, moved, self.body);
                    }
                }
                Some(Drag::Resize { id }) => {
                    if let Some((_, rect)) =
                        self.ws.rects(self.body).into_iter().find(|(p, _)| *p == id)
                    {
                        let resized = Rect {
                            width: (pos.x + 1).saturating_sub(rect.x),
                            height: (pos.y + 1).saturating_sub(rect.y),
                            ..rect
                        };
                        self.ws.set_float_rect(id, resized, self.body);
                    }
                }
                Some(Drag::Select) => {
                    let Some(sel) = self.selection else {
                        return Ok(());
                    };
                    let Some(inner) = self.inner_of(sel.pane) else {
                        return Ok(());
                    };
                    // Dragging past the top or bottom edge scrolls.
                    if let Some(pane) = self.panes.get_mut(&sel.pane) {
                        if pos.y < inner.y {
                            pane.scroll(1);
                        } else if pos.y >= inner.bottom() {
                            pane.scroll(-1);
                        }
                    }
                    let row = pos.y.clamp(inner.y, inner.bottom() - 1) - inner.y;
                    let col = pos.x.clamp(inner.x, inner.right() - 1) - inner.x;
                    let head = self.to_abs(sel.pane, row, col);
                    if let Some(sel) = &mut self.selection {
                        sel.head = head;
                    }
                }
                None => self.forward_to_focused(event)?,
            },
            MouseEventKind::Up(MouseButton::Left) => match self.drag.take() {
                Some(Drag::Select) => {
                    if self.selection.is_some_and(|s| s.anchor != s.head) {
                        self.copy_selection();
                    } else {
                        self.selection = None;
                    }
                }
                Some(_) => {}
                None => self.forward_to_focused(event)?,
            },
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let Some((id, rect)) = self.ws.pane_at(pos, self.body) else {
                    return Ok(());
                };
                let inner = pane_inner(rect);
                if self.forward_mouse(id, event, inner)? {
                    return Ok(());
                }
                let up = event.kind == MouseEventKind::ScrollUp;
                let Some(pane) = self.panes.get_mut(&id) else {
                    return Ok(());
                };
                let (alternate, app_cursor) = {
                    let parser = pane.screen();
                    (
                        parser.screen().alternate_screen(),
                        parser.screen().application_cursor(),
                    )
                };
                if alternate {
                    // Full screen programs without mouse support, like less,
                    // get arrow keys, as most terminals do.
                    let code = if up { KeyCode::Up } else { KeyCode::Down };
                    if let Some(bytes) =
                        keys::encode(KeyEvent::new(code, KeyModifiers::NONE), app_cursor)
                    {
                        for _ in 0..3 {
                            pane.write(&bytes)?;
                        }
                    }
                } else {
                    pane.scroll(if up { 3 } else { -3 });
                }
            }
            _ => self.forward_to_focused(event)?,
        }
        Ok(())
    }

    fn forward_to_focused(&mut self, event: MouseEvent) -> Result<()> {
        if let Some(inner) = self.focused_inner() {
            self.forward_mouse(self.ws.focus, event, inner)?;
        }
        Ok(())
    }

    /// Sends `event` to the program in pane `id` if it asked for mouse
    /// events and the pointer is inside the pane. Returns whether it did.
    fn forward_mouse(&mut self, id: PaneId, event: MouseEvent, inner: Rect) -> Result<bool> {
        let pos = Position::new(event.column, event.row);
        if event.modifiers.contains(KeyModifiers::SHIFT) || !inner.contains(pos) {
            return Ok(false);
        }
        let Some(pane) = self.panes.get_mut(&id) else {
            return Ok(false);
        };
        let (mode, encoding) = {
            let parser = pane.screen();
            (
                parser.screen().mouse_protocol_mode(),
                parser.screen().mouse_protocol_encoding(),
            )
        };
        let Some(bytes) = mouse::encode(
            event.kind,
            event.modifiers,
            pos.x - inner.x,
            pos.y - inner.y,
            mode,
            encoding,
        ) else {
            return Ok(mode != vt100::MouseProtocolMode::None);
        };
        pane.write(&bytes)?;
        Ok(true)
    }

    /// The workspace whose tab is at `pos`, if any.
    fn tab_at(&self, pos: Position) -> Option<u8> {
        let bars = &self.config.bars;
        let layout = screen_layout(self.screen, bars);
        let side = if layout.top.is_some_and(|r| r.contains(pos)) {
            Placement::Top
        } else if layout.bottom.is_some_and(|r| r.contains(pos)) {
            Placement::Bottom
        } else {
            return None;
        };
        if bars.tabs != side {
            return None;
        }
        let mut x = if bars.control() == side {
            BADGE.chars().count() as u16 + 1
        } else {
            0
        };
        for n in self.workspaces() {
            let width = format!(" {n} ").len() as u16;
            if (x..x + width).contains(&pos.x) {
                return Some(n);
            }
            x += width;
        }
        None
    }

    fn perform(&mut self, action: Action) {
        match action {
            Action::ClosePane(id) => self.close(id),
            Action::KillServer => self.quit = true,
        }
    }

    /// Splits the focused pane and focuses the new one. Does nothing if the
    /// pane floats or is too small to be split.
    fn split(&mut self, axis: Axis) {
        let Some((_, rect)) = self
            .ws
            .layout
            .rects(self.body)
            .into_iter()
            .find(|(id, _)| *id == self.ws.focus)
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

        let cwd = self.focused_cwd();
        let id = self.next_id;
        self.next_id += 1;
        self.ws.split(axis, id);
        self.spawn_into_ws(id, cwd);
    }

    /// Starts a shell in a new floating pane in the middle of the screen.
    fn new_float(&mut self) {
        let cwd = self.focused_cwd();
        let id = self.next_id;
        self.next_id += 1;
        self.ws.add_float(id, self.body);
        self.spawn_into_ws(id, cwd);
    }

    /// Starts the shell for pane `id`, which was just added to the active
    /// workspace, sized to the area it got there. Takes the pane out again
    /// if the shell cannot be started.
    fn spawn_into_ws(&mut self, id: PaneId, cwd: Option<PathBuf>) {
        let rect = self
            .ws
            .rects(self.body)
            .into_iter()
            .find_map(|(pane, rect)| (pane == id).then_some(rect))
            .unwrap_or(self.body);
        let inner = pane_inner(rect);
        match Pane::spawn(
            id,
            inner.height,
            inner.width,
            cwd.as_deref(),
            self.events.clone(),
        ) {
            Ok(pane) => {
                self.panes.insert(id, pane);
            }
            Err(e) => {
                eprintln!("failed to start a shell: {e:#}");
                self.ws.remove(id);
            }
        }
    }

    /// The working directory of the focused pane, where new panes start.
    fn focused_cwd(&self) -> Option<PathBuf> {
        self.panes.get(&self.ws.focus).and_then(Pane::cwd)
    }

    /// Removes pane `id`, killing its process if it still runs. A workspace
    /// without panes disappears. When the active one does, another one is
    /// shown, and when none is left, hivemux quits.
    fn close(&mut self, id: PaneId) {
        if self.panes.remove(&id).is_none() {
            return;
        }

        if !self.ws.contains(id) {
            let Some(n) = self
                .hidden
                .iter()
                .find_map(|(n, ws)| ws.contains(id).then_some(*n))
            else {
                return;
            };
            let ws = self.hidden.get_mut(&n).expect("found above");
            ws.remove(id);
            if ws.is_empty() {
                self.hidden.remove(&n);
            }
            return;
        }

        self.ws.remove(id);
        if self.ws.is_empty() {
            // Show the nearest remaining workspace, preferring a lower number.
            let current = self.workspace;
            let nearest = self
                .hidden
                .keys()
                .copied()
                .min_by_key(|n| (n.abs_diff(current), *n > current));
            match nearest {
                Some(n) => self.show(n),
                None => self.quit = true,
            }
        }
    }

    fn workspace_exists(&self, n: u8) -> bool {
        n == self.workspace || self.hidden.contains_key(&n)
    }

    /// Numbers of all workspaces that have panes, in order.
    fn workspaces(&self) -> Vec<u8> {
        let mut all: Vec<u8> = self.hidden.keys().copied().collect();
        all.push(self.workspace);
        all.sort_unstable();
        all
    }

    /// Makes workspace `n` the active one. An empty workspace gets a new
    /// shell, like an empty workspace in i3.
    fn switch_workspace(&mut self, n: u8) {
        if n == self.workspace {
            return;
        }
        if self.hidden.contains_key(&n) {
            self.show(n);
            return;
        }

        let cwd = self.focused_cwd();
        let id = self.next_id;
        let inner = pane_inner(self.body);
        let pane = match Pane::spawn(
            id,
            inner.height,
            inner.width,
            cwd.as_deref(),
            self.events.clone(),
        ) {
            Ok(pane) => pane,
            Err(e) => {
                eprintln!("failed to start a shell for workspace {n}: {e:#}");
                return;
            }
        };
        self.next_id += 1;
        self.panes.insert(id, pane);
        self.stash();
        self.workspace = n;
        self.ws = Workspace::new(id);
    }

    /// Goes to the next or previous workspace that exists, wrapping around.
    fn step_workspace(&mut self, forward: bool) {
        let all = self.workspaces();
        let Some(i) = all.iter().position(|&n| n == self.workspace) else {
            return;
        };
        let len = all.len();
        let next = if forward {
            (i + 1) % len
        } else {
            (i + len - 1) % len
        };
        self.switch_workspace(all[next]);
    }

    /// Brings hidden workspace `n` to the front, hiding the active one unless
    /// it is empty.
    fn show(&mut self, n: u8) {
        let Some(ws) = self.hidden.remove(&n) else {
            return;
        };
        self.stash();
        self.workspace = n;
        self.ws = ws;
    }

    /// Moves the active workspace into `hidden`, if it has any panes.
    fn stash(&mut self) {
        let ws = std::mem::replace(&mut self.ws, Workspace::empty());
        if !ws.is_empty() {
            self.hidden.insert(self.workspace, ws);
        }
    }

    fn paste(&mut self, text: &str) -> Result<()> {
        let Some(pane) = self.panes.get_mut(&self.ws.focus) else {
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
        let screen = screen_layout(frame.area(), &self.config.bars);
        let body = screen.body;

        for (id, rect) in self.ws.rects(body) {
            let Some(pane) = self.panes.get(&id) else {
                continue;
            };
            let focused = id == self.ws.focus;
            let floating = self.ws.is_floating(id);
            let border = if focused {
                Style::new().fg(HONEY)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            let offset = pane.scroll_offset();
            let scrolled = if offset > 0 {
                format!("⇡{offset}/{} ", pane.history_len())
            } else {
                String::new()
            };
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(border)
                .title(if floating {
                    format!(" {id} {} ⧉ {scrolled}", self.title)
                } else {
                    format!(" {id} {} {scrolled}", self.title)
                });
            let inner = block.inner(rect);
            if floating {
                frame.render_widget(Clear, rect);
            }
            frame.render_widget(block, rect);

            let selection = self.selection.filter(|s| s.pane == id).map(|s| {
                (
                    (self.to_view(id, s.anchor.0), s.anchor.1),
                    (self.to_view(id, s.head.0), s.head.1),
                )
            });
            let copy_cursor = self.copy.filter(|c| c.pane == id).and_then(|c| {
                let row = u16::try_from(self.to_view(id, c.pos.0)).ok()?;
                Some(Position::new(inner.x + c.pos.1, inner.y + row))
            });
            let parser = pane.screen();
            let view = ScreenView::new(parser.screen()).selection(selection);
            let cursor = copy_cursor.or_else(|| view.cursor(inner));
            frame.render_widget(view, inner);
            if focused
                && !matches!(
                    self.mode,
                    Mode::Confirm(_) | Mode::Help | Mode::Menu(_) | Mode::Settings(_)
                )
                && (self.mode != Mode::Copy || copy_cursor.is_some())
                && let Some(position) = cursor
            {
                frame.set_cursor_position(position);
            }
        }

        if let Some(area) = screen.top {
            self.draw_bar(frame, area, Placement::Top);
        }
        if let Some(area) = screen.bottom {
            self.draw_bar(frame, area, Placement::Bottom);
        }

        match self.mode {
            Mode::Prefix(menu) if self.config.which_key.enabled => {
                menu::draw_which_key(frame, body, self.config.which_key.position, menu);
            }
            Mode::Settings(selected) => menu::draw_settings(
                frame,
                body,
                &self.config,
                selected,
                self.config_note.as_deref(),
            ),
            Mode::Help => menu::draw_help(frame, body),
            Mode::Menu(selected) => menu::draw_quit_menu(frame, body, &self.menu_items(), selected),
            Mode::Confirm(action) => self.draw_confirm(frame, body, action),
            Mode::Prefix(_) | Mode::Normal | Mode::Repeat(..) | Mode::Copy => {}
        }
    }

    fn menu_items(&self) -> Vec<menu::MenuItem> {
        let shells = self.panes.len();
        MENU.iter()
            .map(|&(key, entry)| match entry {
                MenuEntry::Detach => menu::MenuItem {
                    key,
                    title: "Detach".into(),
                    hint: "shells keep running, `hivemux` brings you back".into(),
                    danger: false,
                },
                MenuEntry::ClosePane => menu::MenuItem {
                    key,
                    title: format!("Close pane {}", self.ws.focus),
                    hint: "ends the program running in it".into(),
                    danger: true,
                },
                MenuEntry::EndSession => menu::MenuItem {
                    key,
                    title: "End session".into(),
                    hint: format!(
                        "closes all {shells} {}",
                        if shells == 1 { "shell" } else { "shells" }
                    ),
                    danger: true,
                },
            })
            .collect()
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

    /// One bar line: control bar and workspace tabs on the left, the path
    /// on the right, whichever of them the settings put on `side`.
    fn draw_bar(&self, frame: &mut Frame, area: Rect, side: Placement) {
        let bars = &self.config.bars;
        let hint = Style::new().add_modifier(Modifier::DIM);
        let control = bars.control() == side;

        let mut spans = Vec::new();
        if control {
            spans.push(Span::styled(BADGE, badge_style()));
            spans.push(Span::raw(" "));
        }
        if bars.tabs == side {
            spans.extend(self.tab_spans());
            spans.push(Span::styled(if control { "│ " } else { " " }, hint));
        }
        if control {
            spans.extend(self.mode_spans());
        }
        let left = Line::from(spans);

        if bars.path == side
            && let Some(cwd) = self.focused_cwd()
        {
            let room = usize::from(area.width).saturating_sub(left.width() + 5);
            let path = shorten(&tilde(&cwd), room);
            if !path.is_empty() {
                // Plain text with a coloured marker: dimmed text is close to
                // invisible in some colour schemes.
                let line = Line::from(vec![
                    Span::styled("▸ ", Style::new().fg(HONEY).add_modifier(Modifier::BOLD)),
                    Span::raw(path),
                    Span::raw(" "),
                ]);
                frame.render_widget(line.right_aligned(), area);
            }
        }
        frame.render_widget(left, area);
    }

    fn tab_spans(&self) -> Vec<Span<'static>> {
        let hint = Style::new().add_modifier(Modifier::DIM);
        self.workspaces()
            .into_iter()
            .map(|n| {
                if n == self.workspace {
                    Span::styled(
                        format!(" {n} "),
                        Style::new()
                            .fg(HONEY)
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled(format!(" {n} "), hint)
                }
            })
            .collect()
    }

    /// The mode badge and key hints of the control bar.
    fn mode_spans(&self) -> Vec<Span<'static>> {
        let badge = badge_style();
        let hint = Style::new().add_modifier(Modifier::DIM);
        let mut spans = Vec::new();
        match self.mode {
            Mode::Prefix(_) => {
                spans.push(Span::styled(" PREFIX ", badge));
                spans.push(Span::styled(
                    "  ? all keys · Esc cancel",
                    Style::new().fg(HONEY),
                ));
            }
            Mode::Menu(_) => {
                spans.push(Span::styled(" QUIT ", badge));
                spans.push(Span::styled(
                    "  ↑↓ select · Enter or letter choose · Esc cancel",
                    Style::new().fg(HONEY),
                ));
            }
            Mode::Settings(_) => {
                spans.push(Span::styled(" SETTINGS ", badge));
                spans.push(Span::styled(
                    "  ↑↓ select · ←→ Enter change · Esc close",
                    Style::new().fg(HONEY),
                ));
            }
            Mode::Copy => {
                spans.push(Span::styled(" COPY ", badge));
                spans.push(Span::styled(
                    "  hjkl move · ^U ^D half page · ^B ^F page · g G top/bottom · v select · y copy · q quit",
                    Style::new().fg(HONEY),
                ));
            }
            Mode::Help => {
                spans.push(Span::styled(" HELP ", badge));
                spans.push(Span::styled("  any key closes", Style::new().fg(HONEY)));
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
            Mode::Repeat(..) => {
                spans.push(Span::styled(" REPEAT ", badge));
                spans.push(Span::styled(
                    "  ←↑↓→ focus  ^←↑↓→ resize",
                    Style::new().fg(HONEY),
                ));
            }
            Mode::Normal if self.flash.is_some() => {
                spans.push(Span::styled(
                    format!("✓ {}", self.flash.as_deref().unwrap_or_default()),
                    Style::new().fg(HONEY),
                ));
            }
            Mode::Normal => {
                spans.push(Span::styled(
                    format!(
                        "{} {} · ^B menu · ^B ? all keys",
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
        spans
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
    bindings::PREFIX.matches(key)
}

/// The screen cut into the bar lines the settings ask for and the panes.
struct ScreenLayout {
    top: Option<Rect>,
    body: Rect,
    bottom: Option<Rect>,
}

fn screen_layout(screen: Rect, bars: &Bars) -> ScreenLayout {
    let top = u16::from(bars.uses(Placement::Top)).min(screen.height);
    let bottom = u16::from(bars.uses(Placement::Bottom)).min(screen.height - top);
    let line = |y| Rect {
        y,
        height: 1,
        ..screen
    };
    ScreenLayout {
        top: (top > 0).then(|| line(screen.y)),
        body: Rect {
            y: screen.y + top,
            height: screen.height - top - bottom,
            ..screen
        },
        bottom: (bottom > 0).then(|| line(screen.bottom() - 1)),
    }
}

const BADGE: &str = " ⬢ hivemux ";

fn badge_style() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(HONEY)
        .add_modifier(Modifier::BOLD)
}

/// `path` cut down to `room` characters, keeping its end, e.g.
/// `…/hivemux/src`.
fn shorten(path: &str, room: usize) -> String {
    let len = path.chars().count();
    if len <= room {
        return path.to_owned();
    }
    if room < 2 {
        return String::new();
    }
    let tail: String = path.chars().skip(len - (room - 1)).collect();
    format!("…{tail}")
}

/// The area inside a pane's border, i.e. the size of its pty.
fn pane_inner(rect: Rect) -> Rect {
    Block::bordered().inner(rect)
}

/// `path` with the home directory written as `~`.
fn tilde(path: &std::path::Path) -> String {
    match std::env::var_os("HOME") {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

fn shell_name() -> String {
    std::env::var("SHELL")
        .ok()
        .and_then(|shell| shell.rsplit('/').next().map(str::to_owned))
        .unwrap_or_else(|| "shell".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_take_lines_on_their_side() {
        let screen = Rect::new(0, 0, 80, 24);
        let layout = screen_layout(screen, &Bars::default());
        assert_eq!(layout.top, None);
        assert_eq!(layout.body, Rect::new(0, 0, 80, 23));
        assert_eq!(layout.bottom, Some(Rect::new(0, 23, 80, 1)));

        let split = Bars {
            control: Placement::Bottom,
            tabs: Placement::Top,
            path: Placement::Off,
        };
        let layout = screen_layout(screen, &split);
        assert_eq!(layout.top, Some(Rect::new(0, 0, 80, 1)));
        assert_eq!(layout.body, Rect::new(0, 1, 80, 22));
        assert_eq!(layout.bottom, Some(Rect::new(0, 23, 80, 1)));
    }

    #[test]
    fn long_paths_keep_their_end() {
        assert_eq!(
            shorten("~/Development/hivemux", 40),
            "~/Development/hivemux"
        );
        assert_eq!(shorten("~/Development/hivemux", 8), "…hivemux");
        assert_eq!(shorten("~/x", 1), "");
    }
}
