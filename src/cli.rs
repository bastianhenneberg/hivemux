//! Commands for scripts and agents: `hivemux status`, `list`, `send`, `new`
//! and `hooks`. They talk to the running server over its socket.

use anyhow::{Context, Result, bail};

use crate::agent::AgentState;
use crate::client;
use crate::layout::PaneId;
use crate::protocol::Request;

/// `hivemux status <working|blocked|idle|clear> [--pane N]`
///
/// Meant for agent hooks, so it stays quiet and succeeds when it runs
/// outside hivemux or the server is gone: a hook must never break the agent.
pub fn status(args: &[String]) -> Result<()> {
    let mut rest = args.to_vec();
    let pane = take_pane(&mut rest)?;
    let [state] = rest.as_slice() else {
        bail!("usage: hivemux status <working|blocked|idle|clear> [--pane N]");
    };
    let state = match state.as_str() {
        "working" => Some(AgentState::Working),
        "blocked" => Some(AgentState::Blocked),
        "idle" => Some(AgentState::Idle),
        "clear" => None,
        other => bail!("unknown state {other:?}, use working, blocked, idle or clear"),
    };
    let Some(pane) = pane else {
        return Ok(());
    };
    let _ = client::request(Request::Status { pane, state });
    Ok(())
}

/// `hivemux list [--json]`
pub fn list(args: &[String]) -> Result<()> {
    let value = client::request(Request::List)?;
    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    println!(
        "{:<5} {:<3} {:<8} {:<14} {:<8} DIRECTORY",
        "PANE", "WS", "", "PROGRAM", "STATE"
    );
    for pane in value.as_array().into_iter().flatten() {
        let text = |key: &str| pane[key].as_str().unwrap_or("-").to_owned();
        let mut flags = Vec::new();
        if pane["focused"].as_bool() == Some(true) {
            flags.push("focused");
        }
        if pane["floating"].as_bool() == Some(true) {
            flags.push("float");
        }
        println!(
            "{:<5} {:<3} {:<8} {:<14} {:<8} {}",
            pane["pane"].to_string(),
            pane["workspace"].to_string(),
            flags.join(","),
            text("program"),
            text("state"),
            text("cwd"),
        );
    }
    Ok(())
}

/// `hivemux send [--pane N] [--no-enter] <text>...`
pub fn send(args: &[String]) -> Result<()> {
    let mut rest = args.to_vec();
    let pane = take_pane(&mut rest)?.context("which pane? use --pane N")?;
    let enter = !take_flag(&mut rest, "--no-enter");
    if rest.is_empty() {
        bail!("usage: hivemux send [--pane N] [--no-enter] <text>...");
    }
    client::request(Request::Send {
        pane,
        text: rest.join(" "),
        enter,
    })?;
    Ok(())
}

/// `hivemux new [--float] [--workspace N] [-- command args...]`
pub fn new(args: &[String]) -> Result<()> {
    let (options, command) = match args.iter().position(|a| a == "--") {
        Some(i) => (args[..i].to_vec(), args[i + 1..].to_vec()),
        None => (args.to_vec(), Vec::new()),
    };
    let mut options = options;
    let float = take_flag(&mut options, "--float");
    let workspace = take_value(&mut options, "--workspace")?
        .map(|n| {
            n.parse::<u8>()
                .context("--workspace takes a number from 1 to 9")
        })
        .transpose()?;
    if let Some(unknown) = options.first() {
        bail!("unknown option {unknown:?}, put the command after --");
    }
    let value = client::request(Request::New {
        command,
        float,
        workspace,
        cwd: std::env::current_dir().ok(),
    })?;
    println!("{}", value["pane"]);
    Ok(())
}

/// `hivemux hooks`: the Claude Code hook config that reports the agent state.
pub fn hooks() -> Result<()> {
    let hook = |state: &str| {
        serde_json::json!([{ "hooks": [{
            "type": "command",
            "command": format!("hivemux status {state}"),
        }]}])
    };
    let config = serde_json::json!({
        "hooks": {
            "UserPromptSubmit": hook("working"),
            "PreToolUse": hook("working"),
            "Notification": hook("blocked"),
            "Stop": hook("idle"),
        }
    });
    println!("{}", serde_json::to_string_pretty(&config)?);
    eprintln!(
        "\nAdd this to ~/.claude/settings.json (merge it with hooks you already have).\n\
         Outside hivemux the hooks do nothing, `hivemux` must be on your PATH."
    );
    Ok(())
}

/// Removes `--pane N` from `args`, falling back to `$HIVEMUX_PANE`, the pane
/// the command runs in.
fn take_pane(args: &mut Vec<String>) -> Result<Option<PaneId>> {
    let value = take_value(args, "--pane")?.or_else(|| std::env::var("HIVEMUX_PANE").ok());
    value
        .map(|v| {
            v.parse()
                .with_context(|| format!("{v:?} is not a pane number"))
        })
        .transpose()
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != flag);
    args.len() != before
}

fn take_value(args: &mut Vec<String>, option: &str) -> Result<Option<String>> {
    let Some(i) = args.iter().position(|a| a == option) else {
        return Ok(None);
    };
    if i + 1 >= args.len() {
        bail!("{option} needs a value");
    }
    let value = args.remove(i + 1);
    args.remove(i);
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn options_are_taken_out() {
        let mut a = args(&["--pane", "4", "hello", "--no-enter", "world"]);
        assert_eq!(take_value(&mut a, "--pane").unwrap(), Some("4".into()));
        assert!(take_flag(&mut a, "--no-enter"));
        assert_eq!(a, args(&["hello", "world"]));
        assert!(take_value(&mut args(&["--pane"]), "--pane").is_err());
    }

    #[test]
    fn status_outside_hivemux_is_a_quiet_no_op() {
        // No --pane and, in the test environment, no HIVEMUX_PANE.
        if std::env::var_os("HIVEMUX_PANE").is_none() {
            assert!(status(&args(&["working"])).is_ok());
        }
        assert!(status(&args(&["sleeping"])).is_err());
    }
}
