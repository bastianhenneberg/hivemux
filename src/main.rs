mod app;
mod client;
mod keys;
mod layout;
mod pane;
mod protocol;
mod render;
mod server;

use anyhow::{Result, bail};

const USAGE: &str = "\
Usage: hivemux [command]

Commands:
  (none)        attach to the running session, or start a new one
  attach        attach to the running session
  kill-server   shut down the server and every pane in it
  help          show this help

Inside hivemux, press Ctrl+B then q (or d) to detach. The shells keep running.";

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
        ["server"] => server::run(),
        ["help" | "-h" | "--help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => bail!("unknown arguments: {}\n\n{USAGE}", args.join(" ")),
    }
}
