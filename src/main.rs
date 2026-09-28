mod agent;
mod app;
mod bindings;
mod cli;
mod client;
mod clipboard;
mod config;
mod keys;
mod layout;
mod menu;
mod mouse;
mod pane;
mod persist;
mod protocol;
mod render;
mod server;
mod workspace;

use anyhow::{Result, bail};

const USAGE: &str = "\
Usage: hivemux [command]

Commands:
  (none)        attach to the running session, or start a new one
  attach        attach to the running session
  kill-server   shut down the server and every pane in it
  keys          list the key bindings

For scripts and agents:
  list [--json]                         list the panes with program and agent state
  status <working|blocked|idle|clear>   report this pane's agent state (for hooks)
  send [--pane N] [--no-enter] <text>   type text into a pane
  new [--float] [--workspace N] [-- command args...]
                                        start a command in a new pane
  hooks                                 print the Claude Code hooks for status
  help          show this help

Inside hivemux, Ctrl+B then q opens the quit menu: detach, close the pane or end the
session. Ctrl+B then d detaches right away, the shells keep running.";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => client::run(true),
        ["attach" | "a"] => client::run(false),
        ["kill-server"] => client::kill_server(),
        ["keys"] => {
            print!("{}", bindings::reference());
            Ok(())
        }
        ["server"] => server::run(),
        ["status", ..] => cli::status(&args[1..]),
        ["list", ..] => cli::list(&args[1..]),
        ["send", ..] => cli::send(&args[1..]),
        ["new", ..] => cli::new(&args[1..]),
        ["hooks"] => cli::hooks(),
        ["help" | "-h" | "--help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => bail!("unknown arguments: {}\n\n{USAGE}", args.join(" ")),
    }
}
