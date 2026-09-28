//! The server's state and event loop. Pty output, client input and client
//! connections all come in over one channel, and after each batch of events
//! the screen is rendered for the attached client.

use std::collections::{BTreeMap, HashMap, HashSet};
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
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use crate::agent::{self, AgentState};
use crate::bindings::{self, Command, Menu};
use crate::config::{self, Bars, Config, Placement, SETTINGS, Side};
use crate::keys;
use crate::layout::{Axis, MIN_PANE_SIZE, PaneId};
use crate::menu;
use crate::pane::{Pane, Spawn};
use crate::persist::{self, Saved, SavedPane};
use crate::protocol::{ClientMsg, Reply, Request, ServerMsg};
use crate::render::ScreenView;
use crate::sidebar::{self, state_style};
use crate::theme;
use crate::workspace::Workspace;
use crate::{clipboard, mouse};

/// How long after a repeatable command (focus, resize) another arrow key
/// repeats it without pressing the prefix again. Same idea as tmux's
/// `repeat-time`.
const REPEAT_TIME: Duration = Duration::from_millis(600);

/// How often the server looks at agent states without other events.
const TICK: Duration = Duration::from_millis(500);

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
    /// Every open connection, attached or not, to answer requests on.
    conns: HashMap<ClientId, UnixStream>,
    next_client_id: ClientId,
    /// The server socket, handed to every pane as `HIVEMUX_SOCKET`.
    socket: PathBuf,
    /// The last agent state seen per pane, and since when it holds.
    agents: HashMap<PaneId, (AgentState, Instant)>,
    /// Agents that finished while the user was not looking, shown as done
    /// until their pane is focused.
    unseen: HashSet<PaneId>,
    /// Where the sidebar is drawn, if it is shown.
    sidebar: Option<Rect>,
    /// Where the layout is saved to survive a restart, see `persist`.
    state_path: Option<PathBuf>,
    /// The Omarchy theme in use when the theme was last applied.
    followed: Option<String>,
    /// What was saved last, to write only when something changed.
    last_saved: String,
    last_save_at: Instant,
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
    pub fn run(events: Sender<AppEvent>, rx: &Receiver<AppEvent>, socket: PathBuf) -> Result<()> {
        let (config, config_error) = Config::load();
        if let Some(e) = &config_error {
            eprintln!("config: {e}");
        }
        let body = screen_layout(DEFAULT_SCREEN, &config.bars).body;

        let mut app = App {
            panes: HashMap::new(),
            ws: Workspace::empty(),
            workspace: 1,
            hidden: BTreeMap::new(),
            next_id: 0,
            screen: DEFAULT_SCREEN,
            body,
            client: None,
            conns: HashMap::new(),
            next_client_id: 0,
            socket,
            agents: HashMap::new(),
            unseen: HashSet::new(),
            sidebar: None,
            state_path: persist::path().ok(),
            followed: None,
            last_saved: String::new(),
            last_save_at: Instant::now(),
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
        app.apply_theme();
        app.restore();
        if app.panes.is_empty() {
            app.start_fresh()?;
        }
        app.event_loop(rx);
        if let Some(mut client) = app.client.take() {
            let _ = client.send(&ServerMsg::Exited);
        }
        // The session was ended on purpose, there is nothing to bring back.
        // A server that dies with the machine never gets here.
        if let Some(path) = &app.state_path {
            persist::remove(path);
        }
        Ok(())
    }

    /// One shell in workspace 1.
    fn start_fresh(&mut self) -> Result<()> {
        let inner = pane_inner(self.body);
        let first = Pane::spawn(
            Spawn {
                id: 0,
                rows: inner.height,
                cols: inner.width,
                cwd: None,
                command: &[],
                socket: &self.socket,
            },
            self.events.clone(),
        )?;
        self.panes.insert(0, first);
        self.ws = Workspace::new(0);
        self.workspace = 1;
        self.hidden.clear();
        self.next_id = 1;
        Ok(())
    }

    /// Brings back the layout saved before the server last died: every
    /// workspace, split and floating pane, each shell in its old directory,
    /// agents with their resume command typed in.
    fn restore(&mut self) {
        let Some(path) = self.state_path.clone() else {
            return;
        };
        let saved = match persist::load(&path) {
            Ok(Some(saved)) => saved,
            Ok(None) => return,
            Err(e) => {
                eprintln!("not restoring the session: {e:#}");
                return;
            }
        };

        let mut workspaces = saved.workspaces;
        for ws in workspaces.values_mut() {
            for (id, rect) in ws.rects(self.body) {
                let spec = saved.panes.get(&id).cloned().unwrap_or_default();
                let inner = pane_inner(rect);
                let spawned = Pane::spawn(
                    Spawn {
                        id,
                        rows: inner.height,
                        cols: inner.width,
                        cwd: spec.cwd.as_deref(),
                        command: &[],
                        socket: &self.socket,
                    },
                    self.events.clone(),
                );
                match spawned {
                    Ok(mut pane) => {
                        if let Some(command) = spec.resume_command() {
                            let _ = pane.write(format!("{command}\r").as_bytes());
                        }
                        pane.session = spec.session.clone();
                        self.panes.insert(id, pane);
                    }
                    Err(e) => {
                        eprintln!("could not restore pane {id}: {e:#}");
                        ws.remove(id);
                    }
                }
            }
        }
        workspaces.retain(|_, ws| !ws.is_empty());
        self.next_id = self.panes.keys().max().map_or(0, |id| id + 1);

        let active = if workspaces.contains_key(&saved.active) {
            Some(saved.active)
        } else {
            workspaces.keys().next().copied()
        };
        let Some(active) = active else {
            return;
        };
        self.ws = workspaces.remove(&active).expect("checked above");
        self.workspace = active;
        self.hidden = workspaces;
        let count = self.panes.len();
        self.flash = Some(format!(
            "restored {count} {} from before the restart",
            if count == 1 { "pane" } else { "panes" }
        ));
    }

    /// Saves the layout when it changed, at most once a second.
    fn save_state(&mut self) {
        let Some(path) = self.state_path.clone() else {
            return;
        };
        if self.last_save_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_save_at = Instant::now();

        let mut workspaces = self.hidden.clone();
        workspaces.insert(self.workspace, self.ws.clone());
        let panes = self
            .panes
            .iter()
            .map(|(&id, pane)| {
                let argv = pane.foreground_argv();
                let saved = SavedPane {
                    cwd: pane.cwd(),
                    agent: agent::agent_name(&argv).map(str::to_owned),
                    session: pane.session.clone(),
                };
                (id, saved)
            })
            .collect();
        let saved = Saved::new(self.workspace, workspaces, panes);
        let Ok(json) = persist::to_json(&saved) else {
            return;
        };
        if json == self.last_saved {
            return;
        }
        match persist::write(&path, &json) {
            Ok(()) => self.last_saved = json,
            Err(e) => eprintln!("could not save the session: {e:#}"),
        }
    }

    fn event_loop(&mut self, rx: &Receiver<AppEvent>) {
        while !self.quit {
            let body = screen_layout(self.screen, &self.config.bars).body;
            (self.body, self.sidebar) = split_sidebar(body, &self.config.sidebar);
            self.sync_sizes();
            self.update_agents();
            self.follow_omarchy();
            self.save_state();
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
                // Wake up now and then to notice agents going quiet.
                _ => match rx.recv_timeout(TICK) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
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
            AppEvent::ClientConnected(stream) => self.connect(stream),
            AppEvent::ClientMsg(_, ClientMsg::KillServer) => {
                self.quit = true;
                Ok(())
            }
            AppEvent::ClientMsg(id, ClientMsg::Attach) => self.attach(id),
            AppEvent::ClientMsg(id, ClientMsg::Request(request)) => {
                let reply = self.request(request);
                match self.conns.get_mut(&id) {
                    Some(stream) => ServerMsg::Reply(reply).write_to(stream).map_err(Into::into),
                    None => Ok(()),
                }
            }
            AppEvent::ClientMsg(id, ClientMsg::Event(event))
                if self.client.as_ref().is_some_and(|c| c.id == id) =>
            {
                self.handle_event(event)
            }
            AppEvent::ClientMsg(..) => Ok(()),
            AppEvent::ClientGone(id) => {
                self.conns.remove(&id);
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

    /// Starts reading from a new connection. It only shows the session once
    /// it sends `ClientMsg::Attach`, so scripts can make requests without
    /// taking the screen from the attached client.
    fn connect(&mut self, stream: UnixStream) -> Result<()> {
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
        self.conns.insert(id, stream);
        Ok(())
    }

    /// Makes connection `id` the attached client, detaching the previous one.
    fn attach(&mut self, id: ClientId) -> Result<()> {
        let Some(stream) = self.conns.get(&id) else {
            return Ok(());
        };
        let stream = stream.try_clone()?;
        self.detach();
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

    fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => self.paste(&text),
            Event::Resize(cols, rows) => self.resize_screen(cols, rows),
            Event::Mouse(event) => self.mouse(event),
            _ => Ok(()),
        }
    }

    /// Answers a request from `hivemux status`, `list`, `send` or `new`.
    fn request(&mut self, request: Request) -> Reply {
        match request {
            Request::Status {
                pane,
                state,
                session,
            } => {
                let p = self.panes.get_mut(&pane).ok_or(format!("no pane {pane}"))?;
                p.reported = state;
                if session.is_some() {
                    p.session = session;
                }
                Ok(serde_json::json!({ "pane": pane }))
            }
            Request::List => Ok(self.list()),
            Request::Send { pane, text, enter } => {
                let p = self.panes.get_mut(&pane).ok_or(format!("no pane {pane}"))?;
                p.note_input();
                let bracketed = p.screen().screen().bracketed_paste();
                let mut bytes = Vec::new();
                if bracketed && text.contains('\n') {
                    bytes.extend_from_slice(b"\x1b[200~");
                    bytes.extend_from_slice(text.as_bytes());
                    bytes.extend_from_slice(b"\x1b[201~");
                } else {
                    bytes.extend_from_slice(text.as_bytes());
                }
                p.write(&bytes).map_err(|e| e.to_string())?;
                if enter {
                    // A separate write, so programs see Enter as a key and
                    // not as part of a paste.
                    thread::sleep(Duration::from_millis(20));
                    p.write(b"\r").map_err(|e| e.to_string())?;
                }
                Ok(serde_json::json!({ "pane": pane }))
            }
            Request::New {
                command,
                float,
                workspace,
                cwd,
            } => self.new_pane(&command, float, workspace, cwd),
        }
    }

    /// Every pane with its workspace, program, state and directory.
    fn list(&self) -> serde_json::Value {
        let mut panes = Vec::new();
        let mut all: Vec<(u8, &Workspace)> = self.hidden.iter().map(|(n, ws)| (*n, ws)).collect();
        all.push((self.workspace, &self.ws));
        all.sort_by_key(|(n, _)| *n);
        for (n, ws) in all {
            for (id, _) in ws.rects(self.body) {
                let Some(pane) = self.panes.get(&id) else {
                    continue;
                };
                panes.push(serde_json::json!({
                    "pane": id,
                    "workspace": n,
                    "floating": ws.is_floating(id),
                    "focused": n == self.workspace && id == self.ws.focus,
                    "program": pane.program(),
                    "state": self.shown_state(id).map(AgentState::name),
                    "cwd": pane.cwd(),
                    "session": pane.session,
                }));
            }
        }
        serde_json::Value::Array(panes)
    }

    /// Starts `command` in a new pane: floating or split off the focused one,
    /// in workspace `n` or the active one. Hidden workspaces stay hidden.
    fn new_pane(
        &mut self,
        command: &[String],
        float: bool,
        workspace: Option<u8>,
        cwd: Option<PathBuf>,
    ) -> Reply {
        let n = workspace.unwrap_or(self.workspace);
        if !(1..=9).contains(&n) {
            return Err(format!("no workspace {n}, there are 1 to 9"));
        }
        let cwd = cwd.or_else(|| self.focused_cwd());
        let id = self.next_id;
        self.next_id += 1;

        let body = self.body;
        let ws = if n == self.workspace {
            &mut self.ws
        } else {
            self.hidden.entry(n).or_insert_with(Workspace::empty)
        };
        if float {
            ws.add_float(id, body);
        } else if ws.is_empty() {
            *ws = Workspace::new(id);
        } else if ws.is_floating(ws.focus) || !ws.split(Axis::Row, id) {
            ws.add_float(id, body);
        }
        let rect = ws
            .rects(body)
            .into_iter()
            .find_map(|(pane, rect)| (pane == id).then_some(rect))
            .unwrap_or(body);

        let inner = pane_inner(rect);
        let spawned = Pane::spawn(
            Spawn {
                id,
                rows: inner.height,
                cols: inner.width,
                cwd: cwd.as_deref(),
                command,
                socket: &self.socket,
            },
            self.events.clone(),
        );
        match spawned {
            Ok(pane) => {
                self.panes.insert(id, pane);
                Ok(serde_json::json!({ "pane": id, "workspace": n }))
            }
            Err(e) => {
                let ws = if n == self.workspace {
                    &mut self.ws
                } else {
                    self.hidden.get_mut(&n).expect("inserted above")
                };
                ws.remove(id);
                if n != self.workspace && ws.is_empty() {
                    self.hidden.remove(&n);
                }
                Err(format!("{e:#}"))
            }
        }
    }

    /// Looks at what every agent is doing. When one starts waiting for the
    /// user where the user is not looking, it rings the bell and says so.
    fn update_agents(&mut self) {
        let now = Instant::now();
        let mut seen = HashMap::new();
        for (&id, pane) in &self.panes {
            let Some(state) = pane.agent_state() else {
                continue;
            };
            let since = match self.agents.get(&id) {
                Some(&(old, since)) if old == state => since,
                _ => now,
            };
            seen.insert(id, (state, since));
        }
        for (&id, &(state, _)) in &seen {
            let was = self.agents.get(&id).map(|(s, _)| *s);
            let in_view = self.ws.contains(id) && (self.ws.focus == id || self.client.is_none());
            if state == AgentState::Idle
                && matches!(was, Some(AgentState::Working | AgentState::Blocked))
                && !in_view
            {
                self.unseen.insert(id);
            }
            if state == AgentState::Blocked && was != Some(AgentState::Blocked) && !in_view {
                self.flash = Some(format!("pane {id} needs you · ^B a"));
                if let Some(client) = &mut self.client {
                    let _ = client.send(&ServerMsg::Output(b"\x07".to_vec()));
                }
            }
        }
        self.unseen.retain(|id| seen.contains_key(id));
        if self.client.is_some() {
            self.unseen.remove(&self.ws.focus);
        }
        self.agents = seen;
    }

    /// The state to show for pane `id`: done while an idle agent has not
    /// been looked at.
    fn shown_state(&self, id: PaneId) -> Option<AgentState> {
        let (state, _) = self.agents.get(&id)?;
        if *state == AgentState::Idle && self.unseen.contains(&id) {
            Some(AgentState::Done)
        } else {
            Some(*state)
        }
    }

    /// The number of the workspace that contains pane `id`.
    fn workspace_of(&self, id: PaneId) -> Option<u8> {
        if self.ws.contains(id) {
            return Some(self.workspace);
        }
        self.hidden
            .iter()
            .find_map(|(n, ws)| ws.contains(id).then_some(*n))
    }

    /// Shows pane `id`, switching to its workspace if needed.
    fn reveal(&mut self, id: PaneId) {
        if !self.ws.contains(id) {
            let Some(n) = self.workspace_of(id) else {
                return;
            };
            self.show(n);
        }
        self.ws.focus(id);
    }

    /// What the sidebar shows.
    fn sidebar_data(&self) -> sidebar::Data {
        let dir_name = |id: PaneId| {
            self.panes
                .get(&id)
                .and_then(Pane::cwd)
                .and_then(|cwd| cwd.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default()
        };
        let workspaces = self
            .workspaces()
            .into_iter()
            .map(|n| {
                let ws = if n == self.workspace {
                    &self.ws
                } else {
                    &self.hidden[&n]
                };
                let states: Vec<AgentState> = ws
                    .panes()
                    .iter()
                    .filter_map(|id| self.shown_state(*id))
                    .collect();
                sidebar::WorkspaceRow {
                    number: n,
                    name: dir_name(ws.focus),
                    active: n == self.workspace,
                    state: states.iter().copied().min_by_key(|s| sidebar::urgency(*s)),
                    agents: states.len(),
                }
            })
            .collect();

        let now = Instant::now();
        let mut agents: Vec<sidebar::AgentRow> = self
            .agents
            .iter()
            .filter_map(|(&id, &(_, since))| {
                let pane = self.panes.get(&id)?;
                Some(sidebar::AgentRow {
                    pane: id,
                    program: pane.program().unwrap_or_default(),
                    workspace: self.workspace_of(id)?,
                    dir: dir_name(id),
                    state: self.shown_state(id)?,
                    since: now - since,
                    focused: id == self.ws.focus,
                })
            })
            .collect();
        // Those that need the user first, and among them the longest waiting.
        agents.sort_by_key(|a| {
            (
                sidebar::urgency(a.state),
                std::cmp::Reverse(a.since),
                a.pane,
            )
        });

        let branch = self.focused_cwd().and_then(|cwd| sidebar::git_branch(&cwd));
        sidebar::Data {
            workspaces,
            agents,
            branch,
        }
    }

    /// Focuses the agent that has been waiting longest, in any workspace.
    /// Pressed again, it goes on to the next one.
    fn jump_to_waiting(&mut self) {
        let mut waiting: Vec<(PaneId, Instant)> = self
            .agents
            .iter()
            .filter(|(_, (state, _))| *state == AgentState::Blocked)
            .map(|(&id, &(_, since))| (id, since))
            .collect();
        waiting.sort_by_key(|&(id, since)| (since, id));
        let Some(&(target, _)) = waiting
            .iter()
            .find(|(id, _)| *id != self.ws.focus)
            .or(waiting.first())
        else {
            self.flash = Some("no agent is waiting".into());
            return;
        };
        self.reveal(target);
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
                    pane.note_input();
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
            Command::JumpToWaiting => self.jump_to_waiting(),
            Command::ToggleSidebar => {
                self.config.sidebar.enabled = !self.config.sidebar.enabled;
                if let Err(e) = self.config.save() {
                    self.config_note = Some(format!("not saved: {e:#}"));
                }
            }
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
                let forward = !matches!(key.code, KeyCode::Left | KeyCode::Char('h'));
                (SETTINGS[selected].step)(&mut self.config, forward);
                self.apply_theme();
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
                if let Some(area) = self.sidebar.filter(|area| area.contains(pos)) {
                    match sidebar::target_at(&self.sidebar_data(), area, pos) {
                        Some(sidebar::Target::Workspace(n)) => self.switch_workspace(n),
                        Some(sidebar::Target::Pane(id)) => self.reveal(id),
                        None => {}
                    }
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

    /// Draws with the configured theme from now on. When it follows
    /// Omarchy, remembers which Omarchy theme that was.
    fn apply_theme(&mut self) {
        self.followed = theme::omarchy_current();
        theme::set(theme::load(&self.config.theme));
    }

    /// Picks up a theme switch on the desktop when following Omarchy.
    fn follow_omarchy(&mut self) {
        if self.config.theme == theme::FOLLOW && theme::omarchy_current() != self.followed {
            self.apply_theme();
        }
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
        let spawned = Pane::spawn(
            Spawn {
                id,
                rows: inner.height,
                cols: inner.width,
                cwd: cwd.as_deref(),
                command: &[],
                socket: &self.socket,
            },
            self.events.clone(),
        );
        match spawned {
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
        let spawned = Pane::spawn(
            Spawn {
                id,
                rows: inner.height,
                cols: inner.width,
                cwd: cwd.as_deref(),
                command: &[],
                socket: &self.socket,
            },
            self.events.clone(),
        );
        let pane = match spawned {
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
        pane.note_input();
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
        let (body, sidebar_area) = split_sidebar(screen.body, &self.config.sidebar);

        for (id, rect) in self.ws.rects(body) {
            let Some(pane) = self.panes.get(&id) else {
                continue;
            };
            let focused = id == self.ws.focus;
            let floating = self.ws.is_floating(id);
            let state = self.shown_state(id);
            let border = match (focused, state) {
                (_, Some(AgentState::Blocked)) => Style::new().fg(theme::current().danger),
                (true, _) => Style::new().fg(theme::current().accent),
                (false, _) => Style::new().fg(theme::current().subtle),
            };

            let mut title = vec![Span::raw(format!(
                " {id} {} ",
                pane.program().unwrap_or_else(|| self.title.clone())
            ))];
            if let Some(state) = state {
                title.push(Span::styled(
                    format!("{} {} ", state.symbol(), state.name()),
                    state_style(state),
                ));
            }
            if floating {
                title.push(Span::raw("⧉ "));
            }
            let offset = pane.scroll_offset();
            if offset > 0 {
                title.push(Span::raw(format!("⇡{offset}/{} ", pane.history_len())));
            }
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(border)
                .title(Line::from(title));
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

        if let Some(area) = sidebar_area {
            sidebar::draw(frame, area, &self.sidebar_data());
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
            .fg(theme::current().danger)
            .add_modifier(Modifier::BOLD);
        let mut text = lines;
        text.push(Line::default());
        text.push(Line::from(vec![
            Span::styled(
                " y ",
                Style::new()
                    .fg(theme::current().on_accent)
                    .bg(theme::current().danger)
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
                    Span::styled(
                        "▸ ",
                        Style::new()
                            .fg(theme::current().accent)
                            .add_modifier(Modifier::BOLD),
                    ),
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
                let ws = if n == self.workspace {
                    &self.ws
                } else {
                    &self.hidden[&n]
                };
                let waiting = self
                    .agents
                    .iter()
                    .any(|(id, (state, _))| *state == AgentState::Blocked && ws.contains(*id));
                if waiting {
                    Span::styled(
                        format!(" {n} "),
                        Style::new()
                            .fg(theme::current().on_accent)
                            .bg(theme::current().danger)
                            .add_modifier(Modifier::BOLD),
                    )
                } else if n == self.workspace {
                    Span::styled(
                        format!(" {n} "),
                        Style::new()
                            .fg(theme::current().accent)
                            .bg(theme::current().subtle)
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
                    Style::new().fg(theme::current().accent),
                ));
            }
            Mode::Menu(_) => {
                spans.push(Span::styled(" QUIT ", badge));
                spans.push(Span::styled(
                    "  ↑↓ select · Enter or letter choose · Esc cancel",
                    Style::new().fg(theme::current().accent),
                ));
            }
            Mode::Settings(_) => {
                spans.push(Span::styled(" SETTINGS ", badge));
                spans.push(Span::styled(
                    "  ↑↓ select · ←→ Enter change · Esc close",
                    Style::new().fg(theme::current().accent),
                ));
            }
            Mode::Copy => {
                spans.push(Span::styled(" COPY ", badge));
                spans.push(Span::styled(
                    "  hjkl move · ^U ^D half page · ^B ^F page · g G top/bottom · v select · y copy · q quit",
                    Style::new().fg(theme::current().accent),
                ));
            }
            Mode::Help => {
                spans.push(Span::styled(" HELP ", badge));
                spans.push(Span::styled(
                    "  any key closes",
                    Style::new().fg(theme::current().accent),
                ));
            }
            Mode::Confirm(_) => {
                spans.push(Span::styled(
                    " CONFIRM ",
                    Style::new()
                        .fg(theme::current().on_accent)
                        .bg(theme::current().danger)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    "  y yes · any other key cancels",
                    Style::new().fg(theme::current().danger),
                ));
            }
            Mode::Repeat(..) => {
                spans.push(Span::styled(" REPEAT ", badge));
                spans.push(Span::styled(
                    "  ←↑↓→ focus  ^←↑↓→ resize",
                    Style::new().fg(theme::current().accent),
                ));
            }
            Mode::Normal if self.flash.is_some() => {
                let text = self.flash.as_deref().unwrap_or_default();
                // Messages about waiting agents are warnings, the rest are
                // confirmations like "copied".
                if self.agents.values().any(|(s, _)| *s == AgentState::Blocked)
                    && text.contains("needs you")
                {
                    spans.push(Span::styled(
                        format!("◆ {text}"),
                        state_style(AgentState::Blocked),
                    ));
                } else {
                    spans.push(Span::styled(
                        format!("✓ {text}"),
                        Style::new().fg(theme::current().accent),
                    ));
                }
            }
            Mode::Normal => {
                for state in [
                    AgentState::Blocked,
                    AgentState::Done,
                    AgentState::Working,
                    AgentState::Idle,
                ] {
                    let count = self
                        .agents
                        .keys()
                        .filter(|id| self.shown_state(**id) == Some(state))
                        .count();
                    if count > 0 {
                        spans.push(Span::styled(
                            format!("{} {count} {}", state.symbol(), state.name()),
                            state_style(state),
                        ));
                        spans.push(Span::styled(" · ", hint));
                    }
                }
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

/// The panes' area and the sidebar's, if it is on and there is room for it
/// next to panes at least as wide.
fn split_sidebar(body: Rect, config: &config::Sidebar) -> (Rect, Option<Rect>) {
    let width = sidebar::WIDTH;
    if !config.enabled || body.width < 2 * width {
        return (body, None);
    }
    let rest = body.width - width;
    match config.side {
        Side::Left => (
            Rect {
                x: body.x + width,
                width: rest,
                ..body
            },
            Some(Rect { width, ..body }),
        ),
        Side::Right => (
            Rect {
                width: rest,
                ..body
            },
            Some(Rect {
                x: body.x + rest,
                width,
                ..body
            }),
        ),
    }
}

fn badge_style() -> Style {
    Style::new()
        .fg(theme::current().on_accent)
        .bg(theme::current().accent)
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
    fn sidebar_takes_its_side_when_there_is_room() {
        let body = Rect::new(0, 1, 120, 30);
        let mut config = config::Sidebar::default();
        let (panes, side) = split_sidebar(body, &config);
        assert_eq!(panes, Rect::new(0, 1, 90, 30));
        assert_eq!(side, Some(Rect::new(90, 1, 30, 30)));

        config.side = Side::Left;
        let (panes, side) = split_sidebar(body, &config);
        assert_eq!(panes, Rect::new(30, 1, 90, 30));
        assert_eq!(side, Some(Rect::new(0, 1, 30, 30)));

        assert_eq!(split_sidebar(Rect::new(0, 0, 50, 20), &config).1, None);
        config.enabled = false;
        assert_eq!(split_sidebar(body, &config), (body, None));
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
