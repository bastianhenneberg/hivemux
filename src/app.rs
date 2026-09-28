//! The event loop: terminal input and pty output come in over one channel,
//! the screen is redrawn after each batch of events.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};

use crate::keys;
use crate::pane::Pane;
use crate::render::ScreenView;

const HONEY: Color = Color::Rgb(250, 190, 0);

pub enum AppEvent {
    Term(Event),
    PtyOutput,
    PtyExited,
}

pub struct App {
    pane: Pane,
    title: String,
    prefix: bool,
    quit: bool,
}

impl App {
    pub fn run(terminal: &mut DefaultTerminal) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        spawn_input_thread(tx.clone())?;

        let size = terminal.size()?;
        let inner = pane_inner(Rect::new(0, 0, size.width, size.height));
        let mut app = App {
            pane: Pane::spawn(inner.height, inner.width, tx)?,
            title: shell_name(),
            prefix: false,
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
            let inner = pane_inner(Rect::new(0, 0, size.width, size.height));
            self.pane.resize(inner.height, inner.width)?;
            terminal.draw(|frame| self.draw(frame))?;

            // Block for the next event, then take everything else that is
            // already queued, so a burst of output costs one redraw.
            let Ok(event) = rx.recv() else { break };
            self.handle(event)?;
            while let Ok(event) = rx.try_recv() {
                self.handle(event)?;
            }
        }
        Ok(())
    }

    fn handle(&mut self, event: AppEvent) -> Result<()> {
        match event {
            AppEvent::Term(Event::Key(key)) => self.handle_key(key)?,
            AppEvent::Term(Event::Paste(text)) => self.paste(&text)?,
            AppEvent::Term(_) | AppEvent::PtyOutput => {}
            AppEvent::PtyExited => self.quit = true,
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind == KeyEventKind::Release {
            return Ok(());
        }

        if self.prefix {
            self.prefix = false;
            match key.code {
                // Prefix twice sends the prefix key itself to the pane.
                _ if is_prefix(key) => self.pane.write(&[0x02])?,
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            }
            return Ok(());
        }

        if is_prefix(key) {
            self.prefix = true;
            return Ok(());
        }

        let app_cursor = self.pane.screen().screen().application_cursor();
        if let Some(bytes) = keys::encode(key, app_cursor) {
            self.pane.write(&bytes)?;
        }
        Ok(())
    }

    fn paste(&mut self, text: &str) -> Result<()> {
        let bracketed = self.pane.screen().screen().bracketed_paste();
        if bracketed {
            self.pane.write(b"\x1b[200~")?;
            self.pane.write(text.as_bytes())?;
            self.pane.write(b"\x1b[201~")
        } else {
            self.pane.write(text.replace('\n', "\r").as_bytes())
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let [pane_area, status_area] = split(frame.area());

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(HONEY))
            .title(format!(" {} ", self.title));
        let inner = block.inner(pane_area);
        frame.render_widget(block, pane_area);

        let parser = self.pane.screen();
        let view = ScreenView::new(parser.screen());
        let cursor = view.cursor(inner);
        frame.render_widget(view, inner);
        if let Some(position) = cursor {
            frame.set_cursor_position(position);
        }

        frame.render_widget(self.status_line(), status_area);
    }

    fn status_line(&self) -> Line<'static> {
        let badge = Style::new()
            .fg(Color::Black)
            .bg(HONEY)
            .add_modifier(Modifier::BOLD);
        let mut spans = vec![Span::styled(" ⬢ hivemux ", badge), Span::raw(" ")];
        if self.prefix {
            spans.push(Span::styled(" PREFIX ", badge));
            spans.push(Span::styled(
                "  q quit · ^B send ^B",
                Style::new().fg(HONEY),
            ));
        } else {
            spans.push(Span::styled(
                "^B q quit",
                Style::new().add_modifier(Modifier::DIM),
            ));
        }
        Line::from(spans)
    }
}

fn is_prefix(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Pane on top, one line of status bar at the bottom.
fn split(area: Rect) -> [Rect; 2] {
    Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area)
}

/// The area inside the pane's border, i.e. the size of the pty.
fn pane_inner(area: Rect) -> Rect {
    let [pane_area, _] = split(area);
    Block::bordered().inner(pane_area)
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
