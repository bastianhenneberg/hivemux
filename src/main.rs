mod app;
mod keys;
mod pane;
mod render;

use std::io::stdout;

use anyhow::Result;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;

fn main() -> Result<()> {
    let mut terminal = ratatui::init();
    execute!(stdout(), EnableBracketedPaste)?;

    let result = app::App::run(&mut terminal);

    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}
