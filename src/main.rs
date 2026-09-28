mod agent;
mod app;
mod bindings;
mod cli;
mod client;
mod clipboard;
mod config;
mod filetree;
mod graphics;
mod keys;
mod layout;
mod menu;
mod mouse;
mod pane;
mod persist;
mod protocol;
mod render;
mod server;
mod sidebar;
mod theme;
mod upgrade;
mod workspace;

use anyhow::{Result, bail};

const USAGE: &str = "\
Usage: hivemux [-s NAME] [command]

-s NAME picks a session other than `default`, e.g. `hivemux -s work`.

Commands:
  (none)        attach to the session, or start it
  attach        attach to the session (`attach -t NAME` works too)
  ls            list the running sessions
  save [NAME]   save every running session's layout in one save file (default `last`)
  restore [NAME | --list]
                start the sessions of a save file that do not run (default `last`,
                which every session ending on purpose updates)
  rename-session NAME
                rename the session (`-s OLD rename-session NEW` for another)
  kill-server   shut down the session's server and every pane in it
  update        move the running server to this binary, e.g. after
                `cargo install`; panes, programs and screens stay
  keys          list the key bindings

For scripts and agents:
  list [--json]                         list the panes with program and agent state
  status <working|blocked|idle|clear>   report this pane's agent state (for hooks)
  send [--pane N] [--no-enter] <text>   type text into a pane
  notify [--title T] [--pane N] <text>  show a notification (desktop, terminal or
                                        off, as set in the settings)
  new [--float] [--workspace N] [-- command args...]
                                        start a command in a new pane
  rename [--pane N | --workspace N] [--clear | <name>]
                                        name a pane or a workspace
  read [--pane N] [--lines N]           print a pane's screen, or its last N lines
  wait [--pane N] --until <state> [--timeout S]
                                        wait for an agent: working, blocked, idle,
                                        done, or ready (idle or done)
  hooks                                 print the Claude Code hooks for status
  help          show this help

Inside hivemux, Ctrl+B then q opens the quit menu: detach, close the pane or end the
session. Ctrl+B then d detaches right away, the shells keep running.";

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // `-s NAME` anywhere, or `attach -t NAME` as in tmux, picks the session.
    if let Some(i) = args.iter().position(|a| {
        a == "-s"
            || a == "--session"
            || (a == "-t" && args.first().is_some_and(|c| c == "attach" || c == "a"))
    }) {
        let Some(name) = args.get(i + 1).cloned() else {
            bail!("{} needs a session name", args[i]);
        };
        if !protocol::valid_session_name(&name) {
            bail!("{name:?} is no session name: letters, digits, - and _ only");
        }
        args.drain(i..=i + 1);
        // SAFETY: nothing else runs yet, no thread can read the
        // environment while it changes.
        unsafe {
            std::env::set_var("HIVEMUX_SESSION", &name);
            std::env::remove_var("HIVEMUX_SOCKET");
        }
    }
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => client::run(true),
        ["attach" | "a"] => client::run(false),
        ["kill-server"] => client::kill_server(),
        ["ls"] => cli::ls(),
        ["keys"] => {
            print!("{}", bindings::reference());
            Ok(())
        }
        ["server"] => server::run(),
        ["status", ..] => cli::status(&args[1..]),
        ["list", ..] => cli::list(&args[1..]),
        ["send", ..] => cli::send(&args[1..]),
        ["notify", ..] => cli::notify(&args[1..]),
        ["new", ..] => cli::new(&args[1..]),
        ["rename", ..] => cli::rename(&args[1..]),
        ["read", ..] => cli::read(&args[1..]),
        ["wait", ..] => cli::wait(&args[1..]),
        ["hooks"] => cli::hooks(),
        ["update"] => cli::update(),
        ["save", ..] => cli::save(&args[1..]),
        ["restore", ..] => cli::restore(&args[1..]),
        ["rename-session", ..] => cli::rename_session(&args[1..]),
        ["help" | "-h" | "--help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => bail!("unknown arguments: {}\n\n{USAGE}", args.join(" ")),
    }
}
