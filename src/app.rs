//! The server's state and event loop. Pty output, client input and client
//! connections all come in over one channel, and after each batch of events
//! the screen is rendered for the attached client.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
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
use crate::config::{self, Bars, Config, Notifications, Placement, SETTINGS, Side};
use crate::filetree::FileTree;
use crate::keys;
use crate::layout::{Axis, MIN_PANE_SIZE, PaneId};
use crate::menu;
use crate::pane::{Pane, Spawn};
use crate::persist::{self, Saved, SavedPane};
use crate::protocol::{self, Caps, ClientMsg, RenameTarget, Reply, Request, ServerMsg};
use crate::render::ScreenView;
use crate::sidebar::{self, state_style};
use crate::theme;
use crate::upgrade::{self, Handover};
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
    /// The key reference is shown, scrolled down by this many lines. j/k
    /// scroll, any other key closes it.
    Help(u16),
    /// The quit menu is open with the given entry selected.
    Menu(usize),
    /// The settings menu is open with the given setting selected.
    Settings(usize),
    /// Scrolling and selecting with the keyboard, see `App::copy`.
    Copy,
    /// Typing a name, see `App::prompt`.
    Prompt,
    /// The sidebar has the keyboard, the given clickable row is selected.
    Sidebar(usize),
    /// The pane picker is open, see `App::picker`.
    Picker,
    /// The session switcher is open with the given entry selected.
    Sessions(usize),
}

/// The pane picker: every pane in every workspace, filtered by what is
/// typed.
struct Picker {
    query: String,
    selected: usize,
}

/// One pane in the picker.
struct PickerEntry {
    pane: PaneId,
    workspace: u8,
    label: String,
    state: Option<AgentState>,
    dir: String,
}

/// A line of text being typed.
struct Prompt {
    purpose: PromptFor,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptFor {
    Rename(RenameTarget),
    /// Searching the copy mode's pane, down or up.
    Search {
        forward: bool,
    },
    /// The name of a session to start and switch to.
    NewSession,
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
    /// Moving the divider between tiled panes at this path in the layout.
    Divider,
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
    /// Focus mode: no bars, no sidebar, no borders, only the focused pane.
    focus_mode: bool,
    /// The server socket's listener, handed on by `hivemux update`.
    listener: RawFd,
    /// The binary `hivemux update` asked to become, once the reply is out.
    upgrade_to: Option<PathBuf>,
    /// The focused pane's repository, for the sidebar.
    git: Option<GitState>,
    /// The focused pane's project, for the sidebar.
    files: FileTree,
    /// How many rows the sidebar is scrolled down.
    sidebar_scroll: usize,
    /// The focused pane as last seen, and the one before it, for `Ctrl+B ;`.
    current_focus: Option<PaneId>,
    last_focus: Option<PaneId>,
    /// Where the layout is saved to survive a restart, see `persist`.
    state_path: Option<PathBuf>,
    /// The Omarchy theme in use when the theme was last applied.
    followed: Option<theme::Stamp>,
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
    /// The divider being dragged, see `Layout::divider_at`.
    divider: Vec<bool>,
    selection: Option<Selection>,
    copy: Option<CopyCursor>,
    /// A short message for the control bar, e.g. what was copied. Cleared by
    /// the next key.
    flash: Option<String>,
    prompt: Option<Prompt>,
    picker: Option<Picker>,
    /// The last copy mode search and its direction, for n and N.
    search: Option<(String, bool)>,
    /// When and where the left button went down last, to spot double clicks.
    last_click: Option<(Instant, Position)>,
    /// The title last sent to the client's terminal window.
    window_title: String,
    /// What the attached client's terminal can do.
    caps: Caps,
    /// Images shown on the client now, by (image, placement), with where.
    shown_images: HashMap<(u32, u32), (u16, u16, u16, u16)>,
    /// Send every image again: a client attached that has none of them.
    resend_images: bool,
    quit: bool,
}

impl App {
    /// Starts with one pane and runs until the last pane is closed or the
    /// server is told to shut down.
    pub fn run(
        events: Sender<AppEvent>,
        rx: &Receiver<AppEvent>,
        socket: PathBuf,
        listener: RawFd,
        handover: Option<Handover>,
    ) -> Result<()> {
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
            focus_mode: false,
            listener,
            upgrade_to: None,
            git: None,
            files: FileTree::default(),
            sidebar_scroll: 0,
            current_focus: None,
            last_focus: None,
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
            divider: Vec::new(),
            selection: None,
            copy: None,
            flash: None,
            prompt: None,
            picker: None,
            search: None,
            last_click: None,
            window_title: String::new(),
            caps: Caps::default(),
            shown_images: HashMap::new(),
            resend_images: false,
            quit: false,
        };
        app.apply_theme();
        match handover {
            Some(handover) => app.adopt(handover),
            None => app.restore(),
        }
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
                        pane.name = spec.name.clone();
                        self.panes.insert(id, pane);
                    }
                    Err(e) => {
                        eprintln!("could not restore pane {id}: {e:#}");
                        ws.remove(id);
                    }
                }
            }
        }
        if !self.install(workspaces, saved.active) {
            return;
        }
        let count = self.panes.len();
        self.flash = Some(format!(
            "restored {count} {} from before the restart",
            if count == 1 { "pane" } else { "panes" }
        ));
    }

    /// Shows `active` of `workspaces`, or the first if it is gone, and
    /// keeps the others hidden. False when no workspace has panes.
    fn install(&mut self, mut workspaces: BTreeMap<u8, Workspace>, active: u8) -> bool {
        workspaces.retain(|_, ws| !ws.is_empty());
        self.next_id = self.panes.keys().max().map_or(0, |id| id + 1);
        let active = if workspaces.contains_key(&active) {
            Some(active)
        } else {
            workspaces.keys().next().copied()
        };
        let Some(active) = active else {
            return false;
        };
        self.ws = workspaces.remove(&active).expect("checked above");
        self.workspace = active;
        self.hidden = workspaces;
        true
    }

    /// Takes over from the server before `hivemux update`: its panes with
    /// their processes and screens, its layout and its attached client.
    fn adopt(&mut self, handover: Handover) {
        self.screen = Rect::new(0, 0, handover.screen.0, handover.screen.1);
        self.focus_mode = handover.focus_mode;
        let saved = handover.saved;
        let mut handed = handover.panes;
        let mut workspaces = saved.workspaces;
        for ws in workspaces.values_mut() {
            for id in ws.panes() {
                let spec = saved.panes.get(&id).cloned().unwrap_or_default();
                match handed
                    .remove(&id)
                    .map(|p| Pane::adopt(id, &p, self.events.clone()))
                {
                    Some(Ok(mut pane)) => {
                        pane.session = spec.session;
                        pane.name = spec.name;
                        self.panes.insert(id, pane);
                    }
                    Some(Err(e)) => {
                        eprintln!("update: could not take over pane {id}: {e:#}");
                        ws.remove(id);
                    }
                    None => ws.remove(id),
                }
            }
        }
        // A pane without a place in the layout ends like a closed one.
        for (id, rest) in handed {
            if let Ok(pane) = Pane::adopt(id, &rest, self.events.clone()) {
                drop(pane);
            }
        }
        if !self.install(workspaces, saved.active) {
            return;
        }
        (_, self.body, self.sidebar) = self.areas(self.screen);
        if let Some(fd) = handover.client {
            let _ = upgrade::set_cloexec(fd, true);
            // SAFETY: the server before handed its client's connection over.
            let stream = unsafe { UnixStream::from_raw_fd(fd) };
            let id = self.next_client_id;
            if let Err(e) = self.connect(stream).and_then(|()| self.attach(id)) {
                eprintln!("update: could not keep the client: {e:#}");
            }
            self.caps = handover.caps;
            if let Some(client) = &mut self.client {
                // Drawn afresh: the new terminal state knows nothing of
                // what the client shows.
                let _ = client.send(&ServerMsg::Output(b"\x1b[H\x1b[2J".to_vec()));
            }
        }
        self.flash = Some(format!(
            "updated to hivemux {}, {} panes kept",
            env!("CARGO_PKG_VERSION"),
            self.panes.len()
        ));
    }

    /// Becomes the binary at `exe` with the session kept, see `upgrade`.
    /// Returns only if that fails.
    fn upgrade(&mut self, exe: &std::path::Path) {
        let mut panes = BTreeMap::new();
        for (&id, pane) in &self.panes {
            let Some(handed) = pane.handover() else {
                self.flash = Some(format!("update: pane {id} cannot be handed over"));
                return;
            };
            panes.insert(id, handed);
        }
        let handover = Handover {
            listener: self.listener,
            client: self.client.as_ref().map(|c| c.stream.as_raw_fd()),
            screen: (self.screen.width, self.screen.height),
            caps: self.caps,
            focus_mode: self.focus_mode,
            saved: self.saved(),
            panes,
        };
        let e = upgrade::exec(exe, &self.socket, &handover);
        eprintln!("update failed: {e:#}");
        self.flash = Some(format!("update failed: {e:#}"));
    }

    /// The layout as `persist` saves it.
    fn saved(&self) -> Saved {
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
                    name: pane.name.clone(),
                };
                (id, saved)
            })
            .collect();
        Saved::new(self.workspace, workspaces, panes)
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

        let Ok(json) = persist::to_json(&self.saved()) else {
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
            (_, self.body, self.sidebar) = self.areas(self.screen);
            self.sync_sizes();
            self.update_agents();
            self.track_focus();
            self.sync_graphics();
            self.refresh_git();
            self.refresh_files();
            self.scroll_sidebar_to_selection();
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
            if let Some(exe) = self.upgrade_to.take() {
                self.upgrade(&exe);
            }
        }
    }

    /// Gives every pty the size of the area inside its pane's border.
    fn sync_sizes(&mut self) {
        for (id, rect) in self.rects_of(&self.ws) {
            let inner = self.inner(rect);
            if let Some(pane) = self.panes.get_mut(&id)
                && let Err(e) = pane.resize(inner.height, inner.width)
            {
                eprintln!("failed to resize pane {id}: {e:#}");
            }
        }
    }

    /// Draws the screen for the attached client. A client that cannot be
    /// written to anymore is dropped.
    fn render(&mut self) {
        // Asked before taking the client out, which makes it look detached.
        let graphics = self.graphics_enabled();
        let Some(mut client) = self.client.take() else {
            return;
        };
        if client.terminal.draw(|frame| self.draw(frame)).is_err() {
            return;
        }
        let images = self.image_output(graphics);
        if !images.is_empty() && client.send(&ServerMsg::Output(images)).is_err() {
            return;
        }
        // The terminal window's title names the workspace and the pane.
        let title = self.window_title_text();
        if title != self.window_title {
            let clean: String = title.chars().filter(|c| !c.is_control()).collect();
            if client
                .send(&ServerMsg::Output(
                    format!("\x1b]2;{clean}\x07").into_bytes(),
                ))
                .is_ok()
            {
                self.window_title = title;
            }
        }
        self.client = Some(client);
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
            AppEvent::ClientMsg(id, ClientMsg::Caps(caps))
                if self.client.as_ref().is_some_and(|c| c.id == id) =>
            {
                self.caps = caps;
                Ok(())
            }
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
        // A new client gets the window title and every image sent afresh.
        self.window_title.clear();
        self.caps = Caps::default();
        self.shown_images.clear();
        self.resend_images = true;
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
            Request::Rename { target, name } => self.rename(target, name),
            Request::Read { pane, lines } => {
                let p = self.panes.get(&pane).ok_or(format!("no pane {pane}"))?;
                Ok(serde_json::json!({ "pane": pane, "text": p.read(lines) }))
            }
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
            Request::Upgrade { exe } => {
                if !exe.is_file() {
                    return Err(format!("{} does not exist", exe.display()));
                }
                self.upgrade_to = Some(exe);
                Ok(serde_json::json!({ "panes": self.panes.len() }))
            }
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
                    "name": pane.name,
                    "workspace_name": ws.name,
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
            // Nobody looks at anything while detached.
            let in_view = self.client.is_some() && self.ws.contains(id) && self.ws.focus == id;
            if state == AgentState::Idle
                && matches!(was, Some(AgentState::Working | AgentState::Blocked))
                && !in_view
            {
                self.unseen.insert(id);
                self.notify(id, "is done");
            }
            if state == AgentState::Blocked && was != Some(AgentState::Blocked) && !in_view {
                self.flash = Some(format!("pane {id} needs you · ^B a"));
                if let Some(client) = &mut self.client {
                    let _ = client.send(&ServerMsg::Output(b"\x07".to_vec()));
                }
                self.notify(id, "needs you");
            }
        }
        self.unseen.retain(|id| seen.contains_key(id));
        if self.client.is_some() {
            self.unseen.remove(&self.ws.focus);
        }
        self.agents = seen;
    }

    /// Tells the user outside hivemux that the agent in pane `id` `what`,
    /// e.g. "needs you", the way the settings say.
    fn notify(&mut self, id: PaneId, what: &str) {
        let Some(pane) = self.panes.get(&id) else {
            return;
        };
        let who = pane
            .name
            .clone()
            .or_else(|| pane.program())
            .unwrap_or_else(|| "agent".into());
        let place = match (self.workspace_of(id), pane.cwd()) {
            (Some(n), Some(cwd)) => format!("workspace {n} · {}", tilde(&cwd)),
            (Some(n), None) => format!("workspace {n}"),
            _ => String::new(),
        };
        let title = format!("{who} {what}");
        match self.config.notifications {
            Notifications::Off => {}
            Notifications::Terminal => {
                if let Some(client) = &mut self.client {
                    let text: String = format!("hivemux: {title}")
                        .chars()
                        .filter(|c| !c.is_control())
                        .collect();
                    let _ = client.send(&ServerMsg::Output(
                        format!("\x1b]9;{text}\x07").into_bytes(),
                    ));
                }
            }
            Notifications::System => {
                let spawned = std::process::Command::new("notify-send")
                    .args(["--app-name=hivemux", &title, &place])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                // Reaped on a thread of its own, so the loop never waits.
                if let Ok(mut child) = spawned {
                    thread::spawn(move || child.wait());
                }
            }
        }
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
                    name: ws.name.clone().unwrap_or_else(|| dir_name(ws.focus)),
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
                    program: pane
                        .name
                        .clone()
                        .or_else(|| pane.program())
                        .unwrap_or_default(),
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
            changes: self
                .git
                .as_ref()
                .map(|g| g.changes.clone())
                .unwrap_or_default(),
            files: self.file_rows(),
        }
    }

    /// The file tree for the sidebar, with changed files marked.
    fn file_rows(&self) -> Option<(String, Vec<crate::filetree::Entry>)> {
        if !self.config.sidebar.files || self.files.root.as_os_str().is_empty() {
            return None;
        }
        let changed: HashSet<PathBuf> = self
            .git
            .as_ref()
            .map(|g| g.changes.iter().map(|c| g.root.join(&c.path)).collect())
            .unwrap_or_default();
        let mut entries = self.files.entries.clone();
        for entry in &mut entries {
            entry.changed = !entry.dir && changed.contains(&entry.path);
        }
        let root = self.files.root.file_name().map_or_else(
            || tilde(&self.files.root),
            |n| n.to_string_lossy().into_owned(),
        );
        Some((root, entries))
    }

    /// Keeps the file tree on the focused pane's project: its repository,
    /// or else its directory.
    fn refresh_files(&mut self) {
        if self.sidebar.is_none() || !self.config.sidebar.files {
            return;
        }
        let root = match (&self.git, self.focused_cwd()) {
            (Some(git), Some(cwd)) if cwd.starts_with(&git.root) => git.root.clone(),
            (_, Some(cwd)) => cwd,
            (_, None) => return,
        };
        self.files.refresh(&root);
    }

    /// Opens or closes a folder of the file tree, or opens a file in
    /// `$EDITOR`. True for a folder, where the sidebar keeps the keyboard.
    fn open_file(&mut self, i: usize) -> bool {
        if self.files.toggle(i) {
            return true;
        }
        let Some(entry) = self.files.entries.get(i) else {
            return false;
        };
        let path = entry.path.to_string_lossy().into_owned();
        let dir = entry.path.parent().map(PathBuf::from);
        let name = format!("edit {}", entry.name);
        self.float_script(&name, "\"${EDITOR:-nvim}\" \"$1\"", &[&path], dir);
        false
    }

    /// Keeps the row the sidebar's keyboard is on in view.
    fn scroll_sidebar_to_selection(&mut self) {
        let (Mode::Sidebar(selected), Some(area)) = (self.mode, self.sidebar) else {
            return;
        };
        let height = area.height.saturating_sub(2);
        let data = self.sidebar_data();
        self.sidebar_scroll = sidebar::scroll_to_show(&data, height, selected, self.sidebar_scroll)
            .min(sidebar::max_scroll(&data, height));
    }

    /// Focuses the agent that has been waiting longest, in any workspace.
    /// Pressed again, it goes on to the next one.
    fn jump_to_waiting(&mut self) {
        // Agents waiting for the user first, then those that finished
        // unseen, each oldest first.
        let mut waiting: Vec<(PaneId, Instant)> = self
            .agents
            .iter()
            .filter(|(id, (state, _))| {
                *state == AgentState::Blocked || self.shown_state(**id) == Some(AgentState::Done)
            })
            .map(|(&id, &(_, since))| (id, since))
            .collect();
        waiting.sort_by_key(|&(id, since)| {
            (self.shown_state(id) != Some(AgentState::Blocked), since, id)
        });
        let Some(&(target, _)) = waiting
            .iter()
            .find(|(id, _)| *id != self.ws.focus)
            .or(waiting.first())
        else {
            self.flash = Some("no agent is waiting or done".into());
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
            Mode::Help(scroll) => {
                self.mode = match key.code {
                    KeyCode::Char('j') | KeyCode::Down => Mode::Help(scroll.saturating_add(1)),
                    KeyCode::Char('k') | KeyCode::Up => Mode::Help(scroll.saturating_sub(1)),
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Mode::Help(scroll.saturating_add(10))
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Mode::Help(scroll.saturating_sub(10))
                    }
                    _ => Mode::Normal,
                };
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
            Mode::Prompt => {
                self.prompt_key(key);
                Ok(())
            }
            Mode::Sidebar(selected) => {
                self.sidebar_key(key, selected);
                Ok(())
            }
            Mode::Picker => {
                self.picker_key(key);
                Ok(())
            }
            Mode::Sessions(selected) => {
                self.sessions_key(key, selected);
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
            if menu == Menu::Commands
                && let KeyCode::Char(c) = key.code
            {
                self.run_user_command(c);
            }
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
            Command::RenamePane => self.start_prompt(RenameTarget::Pane(self.ws.focus)),
            Command::Zoom => self.ws.toggle_zoom(),
            Command::Sessions => {
                let current = protocol::session_name();
                let selected = protocol::sessions()
                    .iter()
                    .position(|s| *s == current)
                    .unwrap_or(0);
                self.mode = Mode::Sessions(selected);
            }
            Command::PickPane => {
                self.picker = Some(Picker {
                    query: String::new(),
                    selected: 0,
                });
                self.mode = Mode::Picker;
            }
            Command::ReloadConfig => {
                let (config, error) = Config::load();
                self.config = config;
                self.apply_theme();
                self.flash = Some(match error {
                    Some(e) => format!("config has an error, using defaults: {e}"),
                    None => "config reloaded".into(),
                });
            }
            Command::ScrollbackEditor => self.scrollback_in_editor(),
            Command::Swap(forward) => self.ws.swap(forward),
            Command::Equalize => self.ws.layout.equalize(),
            Command::LastPane => {
                if let Some(last) = self.last_focus.filter(|id| self.panes.contains_key(id)) {
                    self.reveal(last);
                }
            }
            Command::RenameWorkspace => self.start_prompt(RenameTarget::Workspace(self.workspace)),
            Command::FocusSidebar => self.focus_sidebar(),
            Command::FocusMode => self.focus_mode = !self.focus_mode,
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
            Command::Help => self.mode = Mode::Help(0),
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

    /// The bar lines, the panes' area and the sidebar's. Focus mode leaves
    /// all of the screen to the panes; a focused sidebar still shows.
    fn areas(&self, screen: Rect) -> (ScreenLayout, Rect, Option<Rect>) {
        if self.focus_mode {
            let layout = ScreenLayout {
                top: None,
                body: screen,
                bottom: None,
            };
            let (body, sidebar) = if self.sidebar_focused() {
                split_sidebar(screen, &self.config.sidebar, true)
            } else {
                (screen, None)
            };
            return (layout, body, sidebar);
        }
        let layout = screen_layout(screen, &self.config.bars);
        let (body, sidebar) =
            split_sidebar(layout.body, &self.config.sidebar, self.sidebar_focused());
        (layout, body, sidebar)
    }

    /// The panes of `ws` that are shown, with their areas. In focus mode
    /// that is only the focused one, over all of the panes' area.
    fn rects_of(&self, ws: &Workspace) -> Vec<(PaneId, Rect)> {
        if self.focus_mode {
            vec![(ws.focus, self.body)]
        } else {
            ws.rects(self.body)
        }
    }

    /// The shown pane at `pos`, the topmost where floating panes overlap.
    fn pane_at(&self, pos: Position) -> Option<(PaneId, Rect)> {
        if self.focus_mode {
            return Some((self.ws.focus, self.body)).filter(|(_, r)| r.contains(pos));
        }
        self.ws.pane_at(pos, self.body)
    }

    /// The area of a pane's screen: inside its border, or all of it in
    /// focus mode, which has no borders.
    fn inner(&self, rect: Rect) -> Rect {
        if self.focus_mode {
            rect
        } else {
            pane_inner(rect)
        }
    }

    /// The area inside the focused pane's border.
    fn focused_inner(&self) -> Option<Rect> {
        self.inner_of(self.ws.focus)
    }

    fn inner_of(&self, id: PaneId) -> Option<Rect> {
        self.rects_of(&self.ws)
            .into_iter()
            .find_map(|(pane, rect)| (pane == id).then(|| self.inner(rect)))
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

    /// Moves the copy cursor to the next match of the last search, in its
    /// direction or, with `reverse`, the other way. Wraps around.
    fn search_next(&mut self, reverse: bool) {
        let (Some(copy), Some((query, forward))) = (self.copy, self.search.clone()) else {
            return;
        };
        let Some(pane) = self.panes.get(&copy.pane) else {
            return;
        };
        let rows = pane.rows_text();
        match find_in_rows(&rows, copy.pos, &query, forward != reverse) {
            Some(pos) => {
                let copy = CopyCursor {
                    pane: copy.pane,
                    pos,
                };
                self.copy = Some(copy);
                self.copy_show(copy);
            }
            None => self.flash = Some(format!("{query:?} not found")),
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
            KeyCode::Char('/' | '?') => {
                self.prompt = Some(Prompt {
                    purpose: PromptFor::Search {
                        forward: key.code == KeyCode::Char('/'),
                    },
                    text: String::new(),
                });
                self.mode = Mode::Prompt;
            }
            KeyCode::Char('n') => self.search_next(false),
            KeyCode::Char('N') => self.search_next(true),
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
                    let data = self.sidebar_data();
                    match sidebar::target_at(&data, area, pos, self.sidebar_scroll) {
                        Some(sidebar::Target::Workspace(n)) => self.switch_workspace(n),
                        Some(sidebar::Target::Pane(id)) => self.reveal(id),
                        Some(sidebar::Target::Change(i)) => self.open_change(i, false),
                        Some(sidebar::Target::File(i)) => {
                            self.open_file(i);
                        }
                        None => {}
                    }
                    return Ok(());
                }
                let Some((id, rect)) = self.pane_at(pos) else {
                    return Ok(());
                };
                // A border where two tiled panes meet moves when dragged.
                if !self.ws.is_floating(id)
                    && self.ws.zoomed.is_none()
                    && !self.focus_mode
                    && let Some(path) = self.ws.layout.divider_at(pos, self.body)
                {
                    self.divider = path;
                    self.drag = Some(Drag::Divider);
                    return Ok(());
                }
                if self.copy.is_some_and(|c| c.pane != id) {
                    self.leave_copy();
                }
                self.ws.focus(id);
                if self.ws.is_floating(id) && !self.focus_mode {
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
                let inner = self.inner(rect);
                if !inner.contains(pos) || self.forward_mouse(id, event, inner)? {
                    return Ok(());
                }
                let at = self.to_abs(id, pos.y - inner.y, pos.x - inner.x);
                let double = self
                    .last_click
                    .is_some_and(|(when, at)| at == pos && when.elapsed() < DOUBLE_CLICK);
                self.last_click = Some((Instant::now(), pos));
                if double {
                    self.select_word(id, at);
                    return Ok(());
                }
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
                Some(Drag::Divider) => {
                    let path = self.divider.clone();
                    self.ws.layout.move_divider(&path, pos, self.body);
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
                if let Some(area) = self.sidebar.filter(|area| area.contains(pos)) {
                    let max = sidebar::max_scroll(&self.sidebar_data(), area.height - 2);
                    self.sidebar_scroll = if event.kind == MouseEventKind::ScrollUp {
                        self.sidebar_scroll.saturating_sub(3)
                    } else {
                        (self.sidebar_scroll + 3).min(max)
                    };
                    return Ok(());
                }
                let Some((id, rect)) = self.pane_at(pos) else {
                    return Ok(());
                };
                let inner = self.inner(rect);
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

    /// Selects and copies the word at `at` in pane `id`, for a double click.
    fn select_word(&mut self, id: PaneId, at: (usize, u16)) {
        let Some(pane) = self.panes.get(&id) else {
            return;
        };
        let Some(line) = pane.rows_text().into_iter().nth(at.0) else {
            return;
        };
        let Some((start, end)) = word_bounds(&line, usize::from(at.1)) else {
            return;
        };
        self.selection = Some(Selection {
            pane: id,
            anchor: (at.0, start as u16),
            head: (at.0, end as u16),
        });
        self.copy_selection();
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
        let (layout, ..) = self.areas(self.screen);
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
            badge_text().chars().count() as u16 + 1
        } else {
            0
        };
        for n in self.workspaces() {
            let Some(ws) = self.workspace_ref(n) else {
                continue;
            };
            let width = tab_label(n, ws).chars().count() as u16;
            if (x..x + width).contains(&pos.x) {
                return Some(n);
            }
            x += width;
        }
        None
    }

    /// Remembers the previously focused pane whenever focus moves, however
    /// it moved: keys, mouse, sidebar or workspace switch.
    fn track_focus(&mut self) {
        let now = Some(self.ws.focus);
        if now != self.current_focus {
            if self
                .current_focus
                .is_some_and(|id| self.panes.contains_key(&id))
            {
                self.last_focus = self.current_focus;
            }
            self.current_focus = now;
        }
    }

    /// Every pane matching the picker's query: workspace, name, program,
    /// state and directory are searched, words in any order.
    fn picker_entries(&self, query: &str) -> Vec<PickerEntry> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut all: Vec<(u8, PaneId)> = Vec::new();
        for n in self.workspaces() {
            if let Some(ws) = self.workspace_ref(n) {
                all.extend(ws.panes().into_iter().map(|id| (n, id)));
            }
        }
        all.into_iter()
            .filter_map(|(n, id)| {
                let pane = self.panes.get(&id)?;
                let program = pane.program().unwrap_or_default();
                let label = match &pane.name {
                    Some(name) => format!("{name} ({program})"),
                    None => program,
                };
                let dir = pane.cwd().map(|c| tilde(&c)).unwrap_or_default();
                let state = self.shown_state(id);
                let ws_name = self
                    .workspace_ref(n)
                    .and_then(|w| w.name.clone())
                    .unwrap_or_default();
                let haystack = format!(
                    "{n} {ws_name} {id} {label} {} {dir}",
                    state.map(AgentState::name).unwrap_or_default()
                )
                .to_lowercase();
                words
                    .iter()
                    .all(|w| haystack.contains(w))
                    .then_some(PickerEntry {
                        pane: id,
                        workspace: n,
                        label,
                        state,
                        dir,
                    })
            })
            .collect()
    }

    /// The switcher's entries: every running session, then a new one.
    fn session_items(&self) -> (Vec<String>, Vec<menu::MenuItem>) {
        let current = protocol::session_name();
        let names = protocol::sessions();
        let mut items: Vec<menu::MenuItem> = names
            .iter()
            .enumerate()
            .map(|(i, name)| menu::MenuItem {
                key: char::from_digit((i as u32 + 1) % 10, 10).unwrap_or(' '),
                title: name.clone(),
                hint: if *name == current {
                    "this session".into()
                } else {
                    String::new()
                },
                danger: false,
            })
            .collect();
        items.push(menu::MenuItem {
            key: 'n',
            title: "new session…".into(),
            hint: "`hivemux -s NAME` does the same".into(),
            danger: false,
        });
        (names, items)
    }

    /// The session switcher: arrows or j/k choose, Enter or the digit
    /// switches, n starts a new session.
    fn sessions_key(&mut self, key: KeyEvent, selected: usize) {
        let (names, items) = self.session_items();
        let count = items.len();
        let pick = match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
                return;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode = Mode::Sessions((selected + count - 1) % count);
                return;
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.mode = Mode::Sessions((selected + 1) % count);
                return;
            }
            KeyCode::Enter => selected,
            KeyCode::Char('n') => count - 1,
            KeyCode::Char(c) => match items.iter().position(|item| item.key == c) {
                Some(i) => i,
                None => return,
            },
            _ => return,
        };
        self.mode = Mode::Normal;
        match names.get(pick) {
            Some(name) if *name == protocol::session_name() => {}
            Some(name) => self.switch_session(name.clone()),
            None => {
                self.prompt = Some(Prompt {
                    purpose: PromptFor::NewSession,
                    text: String::new(),
                });
                self.mode = Mode::Prompt;
            }
        }
    }

    /// Sends the client on to session `name`; it starts it if needed.
    fn switch_session(&mut self, name: String) {
        if let Some(mut client) = self.client.take() {
            let _ = client.send(&ServerMsg::Switch(name));
        }
    }

    /// Typing filters, arrows or Ctrl-N/P move, Enter goes to the pane.
    fn picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = &mut self.picker else {
            self.mode = Mode::Normal;
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.picker = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                let query = picker.query.clone();
                let selected = picker.selected;
                self.picker = None;
                self.mode = Mode::Normal;
                if let Some(entry) = self.picker_entries(&query).get(selected) {
                    let id = entry.pane;
                    self.reveal(id);
                }
            }
            KeyCode::Down | KeyCode::Tab => picker.selected += 1,
            KeyCode::Char('n' | 'j') if ctrl => picker.selected += 1,
            KeyCode::Up | KeyCode::BackTab => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Char('p' | 'k') if ctrl => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Char('u' | 'w') if ctrl => {
                picker.query.clear();
                picker.selected = 0;
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.selected = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                picker.query.push(c);
                picker.selected = 0;
            }
            _ => {}
        }
        // Keep the selection on an entry.
        if let Some(picker) = &self.picker {
            let count = self.picker_entries(&picker.query).len();
            if let Some(picker) = &mut self.picker {
                picker.selected = picker.selected.min(count.saturating_sub(1));
            }
        }
    }

    /// The picker in the middle of the screen.
    fn draw_picker(&self, frame: &mut Frame, area: Rect) {
        let Some(picker) = &self.picker else { return };
        let entries = self.picker_entries(&picker.query);
        let accent = theme::current().accent;
        let dim = Style::new().add_modifier(Modifier::DIM);
        let width = area.width.saturating_sub(4).min(90);
        let rows = usize::from(area.height.saturating_sub(8)).max(3);
        let inner_width = usize::from(width.saturating_sub(6));

        let mut lines = vec![
            Line::from(vec![
                Span::styled("› ", Style::new().fg(accent).add_modifier(Modifier::BOLD)),
                Span::raw(picker.query.clone()),
            ]),
            Line::default(),
        ];
        // Scroll the list so the selection stays visible.
        let first = picker.selected.saturating_sub(rows - 1);
        for (i, entry) in entries.iter().enumerate().skip(first).take(rows) {
            let state = entry
                .state
                .map(|s| (format!("{} {}", s.symbol(), s.name()), state_style(s)))
                .unwrap_or_default();
            let left = format!("{} · {:>2}  {}", entry.workspace, entry.pane, entry.label);
            let used = left.chars().count() + state.0.chars().count() + 2;
            let dir = shorten(&entry.dir, inner_width.saturating_sub(used));
            let pad = inner_width.saturating_sub(used + dir.chars().count());
            let mut line = Line::from(vec![
                Span::raw(left),
                Span::raw("  "),
                Span::styled(state.0, state.1),
                Span::raw(" ".repeat(pad)),
                Span::styled(dir, dim),
            ]);
            if i == picker.selected {
                line = line.patch_style(Style::new().bg(theme::current().subtle));
            }
            lines.push(line);
        }
        if entries.is_empty() {
            lines.push(Line::styled("no pane matches", dim));
        }
        lines.push(Line::default());
        lines.push(Line::styled(
            "type to filter · ↑↓ choose · Enter go · Esc cancel",
            dim,
        ));

        let popup = centered(area, width, lines.len() as u16 + 2);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(accent))
            .title(Span::styled(
                format!(" Go to pane · {} ", entries.len()),
                Style::new().fg(accent).add_modifier(Modifier::BOLD),
            ))
            .padding(Padding::horizontal(2));
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(lines).block(block), popup);
        let x = inner.x + 2 + picker.query.chars().count() as u16;
        frame.set_cursor_position(Position::new(
            x.min(inner.right().saturating_sub(1)),
            inner.y,
        ));
    }

    fn sidebar_focused(&self) -> bool {
        matches!(self.mode, Mode::Sidebar(_))
    }

    /// Gives the sidebar the keyboard, with the row of the focused pane's
    /// agent, or else the active workspace, selected.
    fn focus_sidebar(&mut self) {
        let targets = sidebar::targets(&self.sidebar_data());
        let selected = targets
            .iter()
            .position(|t| *t == sidebar::Target::Pane(self.ws.focus))
            .or_else(|| {
                targets
                    .iter()
                    .position(|t| *t == sidebar::Target::Workspace(self.workspace))
            })
            .unwrap_or(0);
        self.mode = Mode::Sidebar(selected);
    }

    /// The sidebar with the keyboard: j/k move, Enter goes there, r names
    /// the row's workspace or pane, x closes the row's pane, Esc leaves. In
    /// the file tree Enter opens a folder or a file, h closes the folder, f
    /// jumps there.
    fn sidebar_key(&mut self, key: KeyEvent, selected: usize) {
        let targets = sidebar::targets(&self.sidebar_data());
        if targets.is_empty() {
            self.mode = Mode::Normal;
            return;
        }
        let selected = selected.min(targets.len() - 1);
        let target = targets[selected];
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'e') => self.mode = Mode::Normal,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.mode = Mode::Sidebar((selected + 1) % targets.len());
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.mode = Mode::Sidebar((selected + targets.len() - 1) % targets.len());
            }
            KeyCode::Char('g') | KeyCode::Home => self.mode = Mode::Sidebar(0),
            KeyCode::Char('G') | KeyCode::End => self.mode = Mode::Sidebar(targets.len() - 1),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                self.mode = Mode::Normal;
                match target {
                    sidebar::Target::Workspace(n) => self.switch_workspace(n),
                    sidebar::Target::Pane(id) => self.reveal(id),
                    sidebar::Target::Change(i) => self.open_change(i, false),
                    sidebar::Target::File(i) => {
                        if self.open_file(i) {
                            self.mode = Mode::Sidebar(selected);
                        }
                    }
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                if let sidebar::Target::File(i) = target
                    && let Some(to) = self.files.collapse(i)
                {
                    let row = targets
                        .iter()
                        .position(|t| *t == sidebar::Target::File(to))
                        .unwrap_or(selected);
                    self.mode = Mode::Sidebar(row);
                }
            }
            KeyCode::Char('f') => {
                if let Some(row) = targets
                    .iter()
                    .position(|t| matches!(t, sidebar::Target::File(_)))
                {
                    self.mode = Mode::Sidebar(row);
                }
            }
            KeyCode::Char('o') => {
                if let sidebar::Target::Change(i) = target {
                    self.mode = Mode::Normal;
                    self.open_change(i, true);
                }
            }
            KeyCode::Char('r') => match target {
                sidebar::Target::Workspace(n) => self.start_prompt(RenameTarget::Workspace(n)),
                sidebar::Target::Pane(id) => self.start_prompt(RenameTarget::Pane(id)),
                sidebar::Target::Change(_) | sidebar::Target::File(_) => {}
            },
            KeyCode::Char('x') => {
                if let sidebar::Target::Pane(id) = target {
                    self.mode = Mode::Confirm(Action::ClosePane(id));
                }
            }
            _ => {}
        }
    }

    /// Opens the name prompt for `target`, filled with its current name.
    fn start_prompt(&mut self, target: RenameTarget) {
        let text = self.name_of(target).unwrap_or_default();
        self.prompt = Some(Prompt {
            purpose: PromptFor::Rename(target),
            text,
        });
        self.mode = Mode::Prompt;
    }

    fn name_of(&self, target: RenameTarget) -> Option<String> {
        match target {
            RenameTarget::Pane(id) => self.panes.get(&id)?.name.clone(),
            RenameTarget::Workspace(n) => self.workspace_ref(n)?.name.clone(),
        }
    }

    fn workspace_ref(&self, n: u8) -> Option<&Workspace> {
        if n == self.workspace {
            Some(&self.ws)
        } else {
            self.hidden.get(&n)
        }
    }

    /// Typing a name: Enter saves it, an empty name clears it, Esc cancels.
    fn prompt_key(&mut self, key: KeyEvent) {
        let Some(prompt) = &mut self.prompt else {
            self.mode = Mode::Normal;
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                let searching = matches!(prompt.purpose, PromptFor::Search { .. });
                self.prompt = None;
                self.mode = if searching { Mode::Copy } else { Mode::Normal };
            }
            KeyCode::Enter => {
                let Prompt { purpose, text } = self.prompt.take().expect("checked above");
                self.mode = Mode::Normal;
                match purpose {
                    PromptFor::Rename(target) => {
                        let name = Some(text).filter(|t| !t.trim().is_empty());
                        if let Err(e) = self.rename(target, name) {
                            self.flash = Some(e);
                        }
                    }
                    PromptFor::NewSession => {
                        let name = text.trim().to_owned();
                        if protocol::valid_session_name(&name) {
                            self.switch_session(name);
                        } else {
                            self.flash = Some("session names use letters, digits, - and _".into());
                        }
                    }
                    PromptFor::Search { forward } => {
                        self.mode = Mode::Copy;
                        if !text.is_empty() {
                            self.search = Some((text, forward));
                            self.search_next(false);
                        }
                    }
                }
            }
            KeyCode::Backspace if ctrl => prompt.text.clear(),
            KeyCode::Char('u' | 'w') if ctrl => prompt.text.clear(),
            KeyCode::Backspace => {
                prompt.text.pop();
            }
            KeyCode::Char(c) if !ctrl && prompt.text.chars().count() < MAX_NAME => {
                prompt.text.push(c);
            }
            _ => {}
        }
    }

    /// Names a pane or a workspace, or clears the name with `None`.
    fn rename(&mut self, target: RenameTarget, name: Option<String>) -> Reply {
        let name = name
            .map(|n| {
                n.chars()
                    .filter(|c| !c.is_control())
                    .take(MAX_NAME)
                    .collect::<String>()
            })
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty());
        match target {
            RenameTarget::Pane(id) => {
                let pane = self.panes.get_mut(&id).ok_or(format!("no pane {id}"))?;
                pane.name = name.clone();
            }
            RenameTarget::Workspace(n) => {
                let ws = if n == self.workspace {
                    &mut self.ws
                } else {
                    self.hidden.get_mut(&n).ok_or(format!("no workspace {n}"))?
                };
                ws.name = name.clone();
            }
        }
        Ok(serde_json::json!({ "name": name }))
    }

    /// Whether images are shown: an attached client whose terminal can, and
    /// the setting on.
    fn graphics_enabled(&self) -> bool {
        self.client.is_some() && self.caps.graphics && self.config.images
    }

    /// Tells every pane whether to handle images and the cell size, and
    /// writes the answers to programs' graphics queries.
    fn sync_graphics(&mut self) {
        let enabled = self.graphics_enabled();
        let cell = (self.caps.cell_width, self.caps.cell_height);
        for pane in self.panes.values_mut() {
            let replies = {
                let mut g = pane.graphics.lock().unwrap();
                g.enabled = enabled;
                g.cell = cell;
                std::mem::take(&mut g.replies)
            };
            for reply in replies {
                let _ = pane.write(&reply);
            }
        }
    }

    /// The bytes that bring the client's images up to date after a frame:
    /// new image data, then placements moved, added and removed. Images
    /// show only in panes nothing is drawn over.
    fn image_output(&mut self, enabled: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for pane in self.panes.values() {
            let mut g = pane.graphics.lock().unwrap();
            if enabled && self.resend_images {
                g.resend();
            }
            for command in g.outbox.drain(..) {
                if enabled {
                    out.extend_from_slice(&command);
                }
            }
        }
        if enabled {
            self.resend_images = false;
        }

        let mut wanted: HashMap<(u32, u32), (u16, u16, u16, u16)> = HashMap::new();
        let quiet = matches!(self.mode, Mode::Normal | Mode::Repeat(..) | Mode::Copy);
        if enabled && quiet {
            let rects = self.rects_of(&self.ws);
            for (i, &(id, rect)) in rects.iter().enumerate() {
                // Anything drawn later, i.e. a floating pane, hides it.
                if rects[i + 1..].iter().any(|(_, r)| r.intersects(rect)) {
                    continue;
                }
                let Some(pane) = self.panes.get(&id) else {
                    continue;
                };
                let inner = pane_inner(rect);
                let top = (pane.history_len() - pane.scroll_offset()) as i64;
                let placements = pane.graphics.lock().unwrap().placements.clone();
                for p in placements {
                    let row = p.row as i64 - top;
                    let fits = row >= 0
                        && row + i64::from(p.rows) <= i64::from(inner.height)
                        && p.col + p.cols <= inner.width;
                    if fits {
                        let at = (inner.x + p.col, inner.y + row as u16, p.cols, p.rows);
                        wanted.insert((p.image, p.id), at);
                    }
                }
            }
        }

        for (&(image, id), &at) in &wanted {
            if self.shown_images.get(&(image, id)) == Some(&at) {
                continue;
            }
            let (x, y, cols, rows) = at;
            // Save the cursor, place the image without moving it, restore.
            out.extend_from_slice(
                format!(
                    "\x1b7\x1b[{};{}H\x1b_Ga=p,i={image},p={id},c={cols},r={rows},C=1,q=2\x1b\\\x1b8",
                    y + 1,
                    x + 1
                )
                .as_bytes(),
            );
        }
        for &(image, id) in self.shown_images.keys() {
            if !wanted.contains_key(&(image, id)) {
                out.extend_from_slice(
                    format!("\x1b_Ga=d,d=i,i={image},p={id},q=2\x1b\\").as_bytes(),
                );
            }
        }
        self.shown_images = wanted;
        out
    }

    /// `hivemux · 2 api · claude`: the workspace, then the focused pane.
    fn window_title_text(&self) -> String {
        let ws = match &self.ws.name {
            Some(name) => format!("{} {name}", self.workspace),
            None => self.workspace.to_string(),
        };
        let pane = self
            .panes
            .get(&self.ws.focus)
            .and_then(|p| p.name.clone().or_else(|| p.program()))
            .unwrap_or_default();
        format!("hivemux · {ws} · {pane}")
    }

    /// Draws with the configured theme from now on. When it follows
    /// Omarchy, remembers which Omarchy theme that was.
    fn apply_theme(&mut self) {
        self.followed = theme::omarchy_stamp();
        theme::set(theme::load(&self.config.theme));
    }

    /// Picks up a theme switch on the desktop when following Omarchy.
    fn follow_omarchy(&mut self) {
        if self.config.theme == theme::FOLLOW && theme::omarchy_stamp() != self.followed {
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
        self.spawn_into_ws(id, cwd, &[]);
    }

    /// Runs `script` with `sh -c` in a new floating pane, in `cwd`. When it
    /// fails, the pane stays until Enter, so the error can be read.
    fn float_script(&mut self, name: &str, script: &str, args: &[&str], cwd: Option<PathBuf>) {
        let wrapped = format!(
            "{script}\nstatus=$?\nif [ $status -ne 0 ]; then printf '\\n[exited with %s, Enter closes]' $status; read _; fi"
        );
        let mut command = vec!["sh".to_owned(), "-c".to_owned(), wrapped, "sh".to_owned()];
        command.extend(args.iter().map(|a| a.to_string()));
        let id = self.next_id;
        self.next_id += 1;
        self.ws.add_float(id, self.body);
        self.spawn_into_ws(id, cwd, &command);
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.name = Some(name.to_owned());
        }
    }

    /// Runs the user's command bound to `key` in the commands menu.
    fn run_user_command(&mut self, key: char) {
        let Some(command) = self.config.commands.iter().find(|c| c.key == key).cloned() else {
            return;
        };
        let cwd = self.focused_cwd();
        if command.float {
            self.float_script(&command.name, &command.command, &[], cwd);
            return;
        }
        let id = self.next_id;
        if !self.ws.split(Axis::Row, id) {
            return;
        }
        self.next_id += 1;
        let script = vec!["sh".to_owned(), "-c".to_owned(), command.command.clone()];
        self.spawn_into_ws(id, cwd, &script);
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.name = Some(command.name);
        }
    }

    /// The diff of changed file `i` in a floating pane, or with `editor` the
    /// file itself in `$EDITOR`.
    fn open_change(&mut self, i: usize, editor: bool) {
        let Some(git) = &self.git else { return };
        let Some(change) = git.changes.get(i) else {
            return;
        };
        let (root, path) = (git.root.clone(), change.path.clone());
        if editor {
            let name = format!("edit {path}");
            self.float_script(&name, "\"${EDITOR:-nvim}\" \"$1\"", &[&path], Some(root));
        } else {
            let name = format!("diff {path}");
            self.float_script(&name, DIFF_SCRIPT, &[&path], Some(root));
        }
    }

    /// The focused pane's whole history in `$EDITOR`, like herdr's prefix+e.
    fn scrollback_in_editor(&mut self) {
        let id = self.ws.focus;
        let Some(pane) = self.panes.get(&id) else {
            return;
        };
        let text = pane.read(Some(usize::MAX));
        let dir = self
            .socket
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let file = dir.join(format!("scrollback-{id}.txt"));
        if let Err(e) = std::fs::write(&file, text + "\n") {
            self.flash = Some(format!("cannot write the scrollback: {e}"));
            return;
        }
        let file = file.to_string_lossy().into_owned();
        self.float_script(
            &format!("history {id}"),
            "\"${EDITOR:-less}\" \"$1\"; rm -f \"$1\"",
            &[&file],
            self.focused_cwd(),
        );
    }

    /// Keeps the focused pane's `git status` for the sidebar, at most every
    /// two seconds and only while the sidebar shows.
    fn refresh_git(&mut self) {
        if self.sidebar.is_none() {
            return;
        }
        let cwd = self.focused_cwd();
        let fresh = self.git.as_ref().is_some_and(|g| {
            Some(&g.cwd) == cwd.as_ref() && g.at.elapsed() < Duration::from_secs(2)
        });
        if fresh {
            return;
        }
        self.git = cwd.and_then(|cwd| git_state(&cwd));
    }

    /// Starts a shell in a new floating pane in the middle of the screen.
    fn new_float(&mut self) {
        let cwd = self.focused_cwd();
        let id = self.next_id;
        self.next_id += 1;
        self.ws.add_float(id, self.body);
        self.spawn_into_ws(id, cwd, &[]);
    }

    /// Starts the shell for pane `id`, which was just added to the active
    /// workspace, sized to the area it got there. Takes the pane out again
    /// if the shell cannot be started.
    fn spawn_into_ws(&mut self, id: PaneId, cwd: Option<PathBuf>, command: &[String]) {
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
                command,
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
        let (screen, body, sidebar_area) = self.areas(frame.area());

        for (id, rect) in self.rects_of(&self.ws) {
            let Some(pane) = self.panes.get(&id) else {
                continue;
            };
            let focused = id == self.ws.focus;
            let floating = self.ws.is_floating(id);
            let state = self.shown_state(id);
            let t = theme::current();
            // Floating panes get their own colour, so they stand apart from
            // the tiling even without focus.
            let border = match (focused, state) {
                (_, Some(AgentState::Blocked)) => Style::new().fg(t.danger),
                (true, _) => Style::new().fg(t.accent),
                (false, _) if floating => Style::new().fg(t.cyan),
                (false, _) => Style::new().fg(t.subtle),
            };
            let label = pane
                .name
                .clone()
                .or_else(|| pane.program())
                .unwrap_or_else(|| self.title.clone());
            let label_style = if focused {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            let mut title = vec![
                Span::styled(format!(" {id} "), Style::new().fg(t.muted)),
                Span::styled(format!("{label} "), label_style),
            ];
            if let Some(state) = state {
                title.push(Span::styled(
                    format!("{} {} ", state.symbol(), state.name()),
                    state_style(state),
                ));
            }
            if floating {
                title.push(Span::styled("⧉ ", Style::new().fg(t.cyan)));
            }
            if self.ws.zoomed == Some(id) {
                title.push(Span::styled(
                    "⤢ zoom ",
                    Style::new()
                        .fg(theme::current().accent)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            let offset = pane.scroll_offset();
            if offset > 0 {
                title.push(Span::styled(
                    format!("⇡{offset}/{} ", pane.history_len()),
                    Style::new().fg(t.magenta),
                ));
            }
            let inner = self.inner(rect);
            if floating {
                frame.render_widget(Clear, rect);
            }
            if !self.focus_mode {
                let block = Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(border)
                    .title(Line::from(title));
                frame.render_widget(block, rect);
            }

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
                    Mode::Confirm(_)
                        | Mode::Help(_)
                        | Mode::Menu(_)
                        | Mode::Settings(_)
                        | Mode::Prompt
                        | Mode::Sidebar(_)
                        | Mode::Picker
                        | Mode::Sessions(_)
                )
                && (self.mode != Mode::Copy || copy_cursor.is_some())
                && let Some(position) = cursor
            {
                frame.set_cursor_position(position);
            }
        }

        if let Some(area) = sidebar_area {
            let selected = match self.mode {
                Mode::Sidebar(i) => Some(i),
                _ => None,
            };
            sidebar::draw(
                frame,
                area,
                &self.sidebar_data(),
                selected,
                self.sidebar_scroll,
            );
        }
        if let Some(area) = screen.top {
            self.draw_bar(frame, area, Placement::Top);
        }
        if let Some(area) = screen.bottom {
            self.draw_bar(frame, area, Placement::Bottom);
        }
        if self.focus_mode {
            self.draw_focus_overlay(frame, frame.area());
        }

        match self.mode {
            Mode::Prefix(menu) if self.config.which_key.enabled => {
                let commands: Vec<(char, String)> = self
                    .config
                    .commands
                    .iter()
                    .map(|c| (c.key, c.name.clone()))
                    .collect();
                menu::draw_which_key(frame, body, self.config.which_key.position, menu, &commands);
            }
            Mode::Settings(selected) => menu::draw_settings(
                frame,
                body,
                &self.config,
                selected,
                self.config_note.as_deref(),
            ),
            Mode::Help(scroll) => menu::draw_help(frame, body, scroll),
            Mode::Menu(selected) => menu::draw_quit_menu(frame, body, &self.menu_items(), selected),
            Mode::Confirm(action) => self.draw_confirm(frame, body, action),
            Mode::Prompt => self.draw_prompt(frame, body),
            Mode::Picker => self.draw_picker(frame, body),
            Mode::Sessions(selected) => {
                let (_, items) = self.session_items();
                menu::draw_choices(frame, body, " ⬢ Sessions ", &items, selected);
            }
            Mode::Prefix(_) | Mode::Normal | Mode::Repeat(..) | Mode::Copy | Mode::Sidebar(_) => {}
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

    /// The name prompt in the middle of the screen, with the terminal cursor
    /// at the end of the text.
    fn draw_prompt(&self, frame: &mut Frame, area: Rect) {
        let Some(prompt) = &self.prompt else { return };
        let title = match prompt.purpose {
            PromptFor::Rename(RenameTarget::Pane(id)) => format!(" Name pane {id} "),
            PromptFor::Rename(RenameTarget::Workspace(n)) => format!(" Name workspace {n} "),
            PromptFor::Search { forward: true } => " Search down ".to_owned(),
            PromptFor::Search { forward: false } => " Search up ".to_owned(),
            PromptFor::NewSession => " New session ".to_owned(),
        };
        let accent = theme::current().accent;
        let hint = Style::new().add_modifier(Modifier::DIM);
        let lines = vec![
            Line::from(vec![
                Span::styled("› ", Style::new().fg(accent).add_modifier(Modifier::BOLD)),
                Span::raw(prompt.text.clone()),
            ]),
            Line::default(),
            Line::styled(
                match prompt.purpose {
                    PromptFor::Rename(_) => "Enter save · empty clears the name · Esc cancel",
                    PromptFor::Search { .. } => "Enter find · then n next, N previous · Esc cancel",
                    PromptFor::NewSession => "Enter start and switch · letters, digits, - and _",
                },
                hint,
            ),
        ];
        let width = (MAX_NAME as u16 + 8).max(56);
        let popup = centered(area, width, lines.len() as u16 + 2);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(accent))
            .title(Span::styled(
                title,
                Style::new().fg(accent).add_modifier(Modifier::BOLD),
            ))
            .padding(Padding::horizontal(2));
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(lines).block(block), popup);
        let x = inner.x + 2 + prompt.text.chars().count() as u16;
        frame.set_cursor_position(Position::new(
            x.min(inner.right().saturating_sub(1)),
            inner.y,
        ));
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
    /// What focus mode keeps of the bars: the control line while a menu or
    /// the prefix is up, and a corner note when an agent elsewhere waits.
    fn draw_focus_overlay(&self, frame: &mut Frame, screen: Rect) {
        if !matches!(self.mode, Mode::Normal) {
            let side = self.config.bars.control();
            let y = match side {
                Placement::Top => screen.y,
                _ => screen.bottom().saturating_sub(1),
            };
            let line = Rect {
                y,
                height: 1.min(screen.height),
                ..screen
            };
            frame.render_widget(Clear, line);
            self.draw_bar(frame, line, side);
        }
        let waiting = self
            .panes
            .keys()
            .filter(|&&id| id != self.ws.focus)
            .filter(|&&id| self.shown_state(id) == Some(AgentState::Blocked))
            .count();
        if waiting > 0 {
            let text = format!(" {} {waiting} waiting ", AgentState::Blocked.symbol());
            let width = (text.chars().count() as u16).min(screen.width);
            let area = Rect {
                x: screen.right() - width,
                y: screen.y,
                width,
                height: 1.min(screen.height),
            };
            let t = theme::current();
            frame.render_widget(
                Paragraph::new(text).style(Style::new().fg(t.on_accent).bg(t.danger)),
                area,
            );
        }
    }

    fn draw_bar(&self, frame: &mut Frame, area: Rect, side: Placement) {
        let bars = &self.config.bars;
        let t = theme::current();
        let hint = Style::new().fg(t.muted);
        // The bar gets a surface of its own, set apart from the panes.
        frame.render_widget(Block::new().style(Style::new().bg(t.surface)), area);
        let control = bars.control() == side;

        let mut spans = Vec::new();
        if control {
            spans.push(Span::styled(badge_text(), badge_style()));
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
                    Span::styled("▸ ", Style::new().fg(t.cyan).add_modifier(Modifier::BOLD)),
                    Span::raw(path),
                    Span::raw(" "),
                ]);
                // Alone in its line the path starts on the left, where the
                // eye is. Next to the control bar or tabs it goes right.
                if left.width() == 0 {
                    let area = Rect {
                        x: area.x + 1,
                        width: area.width.saturating_sub(1),
                        ..area
                    };
                    frame.render_widget(line, area);
                } else {
                    frame.render_widget(line.right_aligned(), area);
                }
            }
        }
        frame.render_widget(left, area);
    }

    /// Workspace tabs, each in its workspace's colour: filled when it is
    /// the active one, red when an agent there waits.
    fn tab_spans(&self) -> Vec<Span<'static>> {
        let t = theme::current();
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
                        tab_label(n, ws),
                        Style::new()
                            .fg(theme::current().on_accent)
                            .bg(theme::current().danger)
                            .add_modifier(Modifier::BOLD),
                    )
                } else if n == self.workspace {
                    Span::styled(
                        tab_label(n, ws),
                        Style::new()
                            .fg(t.on_accent)
                            .bg(t.workspace(n))
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled(tab_label(n, ws), Style::new().fg(t.workspace(n)))
                }
            })
            .collect()
    }

    /// The mode badge and key hints of the control bar. Every mode has its
    /// own colour, so a glance at the bar tells which one is on.
    fn mode_spans(&self) -> Vec<Span<'static>> {
        let t = theme::current();
        let hint = Style::new().fg(t.muted);
        let mut spans = Vec::new();
        let searching = self
            .prompt
            .as_ref()
            .is_some_and(|p| matches!(p.purpose, PromptFor::Search { .. }));
        let (label, color, keys) = match self.mode {
            Mode::Prefix(_) => ("PREFIX", t.accent, "? all keys · Esc cancel"),
            Mode::Menu(_) => (
                "QUIT",
                t.magenta,
                "↑↓ select · Enter or letter choose · Esc cancel",
            ),
            Mode::Settings(_) => (
                "SETTINGS",
                t.cyan,
                "↑↓ select · ←→ Enter change · Esc close",
            ),
            Mode::Sessions(_) => (
                "SESSIONS",
                t.magenta,
                "↑↓ choose · Enter switch · n new · Esc cancel",
            ),
            Mode::Picker => ("GO TO", t.cyan, "type to filter · Enter go · Esc cancel"),
            Mode::Sidebar(_) => (
                "SIDEBAR",
                t.blue,
                "j k move · Enter go/open · h fold · f files · o edit · r name · x close · Esc back",
            ),
            Mode::Prompt if searching => ("SEARCH", t.orange, "Enter find · Esc cancel"),
            Mode::Prompt => ("NAME", t.orange, "Enter save · Esc cancel"),
            Mode::Copy => (
                "COPY",
                t.magenta,
                "hjkl move · ^U ^D ^B ^F page · g G ends · / ? search · n N next · v select · y copy · q quit",
            ),
            Mode::Help(_) => ("HELP", t.blue, "j k scroll · any other key closes"),
            Mode::Confirm(_) => ("CONFIRM", t.danger, "y yes · any other key cancels"),
            Mode::Repeat(..) => ("REPEAT", t.orange, "arrows again, without the prefix"),
            Mode::Normal => ("", t.accent, ""),
        };
        if !label.is_empty() {
            spans.push(Span::styled(
                format!(" {label} "),
                Style::new()
                    .fg(t.on_accent)
                    .bg(color)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(format!("  {keys}"), Style::new().fg(color)));
            return spans;
        }
        match self.mode {
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
                        Style::new().fg(theme::current().success),
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
            // Every other mode returned its badge above.
            _ => {}
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

/// ` ⬢ hivemux `, or the session's name for a named one.
fn badge_text() -> String {
    let name = protocol::session_name();
    if name == protocol::DEFAULT_SESSION {
        " ⬢ hivemux ".to_owned()
    } else {
        format!(" ⬢ {name} ")
    }
}

/// The panes' area and the sidebar's, if it is on and there is room for it
/// next to panes at least as wide.
/// A focused sidebar is shown even when it is turned off.
fn split_sidebar(body: Rect, config: &config::Sidebar, focused: bool) -> (Rect, Option<Rect>) {
    let width = sidebar::WIDTH;
    if !(config.enabled || focused) || body.width < 2 * width {
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

/// Shows a file's changes against the last commit, or all of it when git
/// does not track it yet, in a pager.
const DIFF_SCRIPT: &str = "if git ls-files --error-unmatch -- \"$1\" >/dev/null 2>&1; \
then git diff --color=always HEAD -- \"$1\"; \
else git diff --color=always --no-index -- /dev/null \"$1\"; fi | less -R";

/// A repository's changed files, as last read.
struct GitState {
    /// The directory it was read for.
    cwd: PathBuf,
    root: PathBuf,
    changes: Vec<sidebar::Change>,
    at: Instant,
}

/// Reads the repository containing `cwd`, `None` outside one.
fn git_state(cwd: &std::path::Path) -> Option<GitState> {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let root = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?.trim());
    let status = git(&["status", "--porcelain"])?;
    Some(GitState {
        cwd: cwd.to_owned(),
        root,
        changes: sidebar::parse_changes(&status),
        at: Instant::now(),
    })
}

/// Two clicks on the same cell within this time make a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// The first and last column of the word around column `col` in `line`,
/// `None` on a space.
fn word_bounds(line: &str, col: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    let is_word = |c: &char| !c.is_whitespace() && !"\"'`()[]{}<>|,;".contains(*c);
    if !chars.get(col).is_some_and(is_word) {
        return None;
    }
    let start = chars[..col]
        .iter()
        .rposition(|c| !is_word(c))
        .map_or(0, |i| i + 1);
    let end = chars[col..]
        .iter()
        .position(|c| !is_word(c))
        .map_or(chars.len(), |i| col + i)
        - 1;
    Some((start, end))
}

/// The next place `query` appears in `rows` after `from` (before it when
/// not `forward`), wrapping around. Ignores case unless the query has
/// capitals, like vim's smartcase.
fn find_in_rows(
    rows: &[String],
    from: (usize, u16),
    query: &str,
    forward: bool,
) -> Option<(usize, u16)> {
    let fold = !query.chars().any(char::is_uppercase);
    let norm = |s: &str| if fold { s.to_lowercase() } else { s.to_owned() };
    let query = norm(query);
    let len = rows.len();
    if len == 0 || query.is_empty() {
        return None;
    }
    // Columns of every match in a row, in characters.
    let matches = |row: &str| -> Vec<usize> {
        let row = norm(row);
        row.match_indices(&query)
            .map(|(byte, _)| row[..byte].chars().count())
            .collect()
    };
    let (row0, col0) = (from.0.min(len - 1), usize::from(from.1));
    for step in 0..=len {
        let row = if forward {
            (row0 + step) % len
        } else {
            (row0 + len - step % len) % len
        };
        let cols = matches(&rows[row]);
        let hit = match (step, forward) {
            (0, true) => cols.into_iter().find(|&c| c > col0),
            (0, false) => cols.into_iter().rev().find(|&c| c < col0),
            (_, true) => cols.into_iter().next(),
            (_, false) => cols.into_iter().next_back(),
        };
        if let Some(col) = hit {
            return Some((row, col as u16));
        }
    }
    None
}

/// Names are cut to this many characters.
const MAX_NAME: usize = 32;

/// A workspace tab: its number, and its name if it has one.
fn tab_label(n: u8, ws: &Workspace) -> String {
    match &ws.name {
        Some(name) => format!(" {n} {name} "),
        None => format!(" {n} "),
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
        let (panes, side) = split_sidebar(body, &config, false);
        assert_eq!(panes, Rect::new(0, 1, 90, 30));
        assert_eq!(side, Some(Rect::new(90, 1, 30, 30)));

        config.side = Side::Left;
        let (panes, side) = split_sidebar(body, &config, false);
        assert_eq!(panes, Rect::new(30, 1, 90, 30));
        assert_eq!(side, Some(Rect::new(0, 1, 30, 30)));

        assert_eq!(
            split_sidebar(Rect::new(0, 0, 50, 20), &config, false).1,
            None
        );
        config.enabled = false;
        assert_eq!(split_sidebar(body, &config, false), (body, None));
        assert!(split_sidebar(body, &config, true).1.is_some());
    }

    #[test]
    fn double_click_finds_the_word() {
        let line = "cargo test --lib (src/app.rs) done";
        assert_eq!(word_bounds(line, 2), Some((0, 4)));
        assert_eq!(word_bounds(line, 12), Some((11, 15)));
        assert_eq!(word_bounds(line, 20), Some((18, 27)));
        assert_eq!(word_bounds(line, 5), None);
    }

    #[test]
    fn search_finds_matches_both_ways_and_wraps() {
        let rows: Vec<String> = ["error one", "ok", "Error two", "ok"]
            .map(String::from)
            .to_vec();
        assert_eq!(find_in_rows(&rows, (0, 0), "error", true), Some((2, 0)));
        assert_eq!(find_in_rows(&rows, (2, 0), "error", true), Some((0, 0)));
        assert_eq!(find_in_rows(&rows, (3, 0), "error", false), Some((2, 0)));
        assert_eq!(find_in_rows(&rows, (3, 0), "Error", false), Some((2, 0)));
        assert_eq!(find_in_rows(&rows, (2, 0), "Error", true), Some((2, 0)));
        assert_eq!(find_in_rows(&rows, (0, 0), "missing", true), None);
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
