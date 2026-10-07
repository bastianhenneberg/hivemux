//! Commands for scripts and agents: `hivemux status`, `list`, `send`, `new`,
//! `notify` and `hooks`. They talk to the running server over its socket.

use std::io::Read;

use anyhow::{Context, Result, bail};

use crate::agent::AgentState;
use crate::client;
use crate::layout::PaneId;
use crate::protocol::{self, RenameTarget, Request};

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
    // Agent hooks pass what happened as JSON on stdin.
    let hook = read_hook_input();
    if hook.as_ref().is_some_and(|hook| ignore_hook(hook, state)) {
        return Ok(());
    }
    let session = hook
        .as_ref()
        .and_then(|hook| hook["session_id"].as_str())
        .map(str::to_owned);
    let _ = client::request(Request::Status {
        pane,
        state,
        session,
    });
    Ok(())
}

/// The JSON a hook got on stdin, `None` when stdin is a terminal or holds
/// no JSON.
fn read_hook_input() -> Option<serde_json::Value> {
    // SAFETY: isatty only looks at the file descriptor.
    if unsafe { libc::isatty(0) } == 1 {
        return None;
    }
    let mut text = String::new();
    std::io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut text)
        .ok()?;
    serde_json::from_str(&text).ok()
}

/// Hook events that must not change the pane's state: a subagent finishing
/// is not the agent finishing, so it must not make the pane look done.
fn ignore_hook(hook: &serde_json::Value, state: Option<AgentState>) -> bool {
    if hook["hook_event_name"] == "SubagentStop" {
        return true;
    }
    let subagent = hook.get("agent_id").is_some_and(|id| !id.is_null());
    subagent && matches!(state, Some(AgentState::Idle) | None)
}

/// `hivemux ls`: the running sessions with their panes and agents.
pub fn ls() -> Result<()> {
    let current = protocol::session_name();
    let names = protocol::sessions();
    if names.is_empty() {
        println!("no sessions running");
        return Ok(());
    }
    for name in names {
        let path = protocol::session_socket(&name)?;
        let marker = if name == current { "*" } else { " " };
        let Ok(panes) = client::request_at(&path, Request::List) else {
            println!("{marker} {name:<16} (not answering)");
            continue;
        };
        let panes = panes.as_array().cloned().unwrap_or_default();
        let mut workspaces: Vec<u64> = panes
            .iter()
            .filter_map(|p| p["workspace"].as_u64())
            .collect();
        workspaces.sort_unstable();
        workspaces.dedup();
        let mut agents = String::new();
        for state in [
            AgentState::Blocked,
            AgentState::Done,
            AgentState::Working,
            AgentState::Idle,
        ] {
            let count = panes.iter().filter(|p| p["state"] == state.name()).count();
            if count > 0 {
                agents.push_str(&format!("  {} {count} {}", state.symbol(), state.name()));
            }
        }
        println!(
            "{marker} {name:<16} {} panes in {} windows{agents}",
            panes.len(),
            workspaces.len()
        );
    }
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
        "{:<5} {:<3} {:<8} {:<26} {:<8} DIRECTORY",
        "PANE", "WIN", "", "NAME", "STATE"
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
            "{:<5} {:<3} {:<8} {:<26} {:<8} {}",
            pane["pane"].to_string(),
            pane["workspace"].to_string(),
            flags.join(","),
            match pane["name"].as_str() {
                Some(name) => format!("{name} ({})", text("program")),
                None => text("program"),
            },
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

/// `hivemux notify [--title T] [--pane N] <text>...`
///
/// Shows `text` as a notification, the way the settings say, and in the
/// control bar. Without a title it is titled after the pane it runs in.
pub fn notify(args: &[String]) -> Result<()> {
    let mut rest = args.to_vec();
    let title = take_value(&mut rest, "--title")?;
    let pane = take_pane(&mut rest)?;
    if rest.is_empty() {
        bail!("usage: hivemux notify [--title T] [--pane N] <text>...");
    }
    client::request(Request::Notify {
        text: rest.join(" "),
        title,
        pane,
    })?;
    Ok(())
}

/// `hivemux read [--pane N] [--lines N]`: what is on a pane's screen, or its
/// last N lines with the history.
pub fn read(args: &[String]) -> Result<()> {
    let mut rest = args.to_vec();
    let pane = take_pane(&mut rest)?.context("which pane? use --pane N")?;
    let lines = take_value(&mut rest, "--lines")?
        .map(|n| n.parse::<usize>().context("--lines takes a number"))
        .transpose()?;
    let value = client::request(Request::Read { pane, lines })?;
    println!("{}", value["text"].as_str().unwrap_or_default());
    Ok(())
}

/// `hivemux wait [--pane N] --until <working|blocked|idle|done|ready> [--timeout S]`
///
/// Waits until the agent in a pane reaches a state, for scripts and agents
/// that hand work to another agent. `ready` means idle or done. Exits with
/// an error on timeout, or when the pane is gone.
pub fn wait(args: &[String]) -> Result<()> {
    let mut rest = args.to_vec();
    let pane = take_pane(&mut rest)?.context("which pane? use --pane N")?;
    let until = take_value(&mut rest, "--until")?.context(
        "usage: hivemux wait [--pane N] --until <working|blocked|idle|done|ready> [--timeout S]",
    )?;
    let wanted: &[&str] = match until.as_str() {
        "ready" => &["idle", "done"],
        "working" => &["working"],
        "blocked" => &["blocked"],
        "idle" => &["idle"],
        "done" => &["done"],
        other => bail!("unknown state {other:?}, use working, blocked, idle, done or ready"),
    };
    let timeout = take_value(&mut rest, "--timeout")?
        .map(|s| s.parse::<f64>().context("--timeout takes seconds"))
        .transpose()?
        .map(std::time::Duration::from_secs_f64);

    let start = std::time::Instant::now();
    loop {
        let panes = client::request(Request::List)?;
        let Some(entry) = panes
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["pane"].as_u64() == Some(pane as u64))
        else {
            bail!("pane {pane} is gone");
        };
        if let Some(state) = entry["state"].as_str()
            && wanted.contains(&state)
        {
            println!("{state}");
            return Ok(());
        }
        if timeout.is_some_and(|t| start.elapsed() >= t) {
            bail!("pane {pane} did not become {until} in time");
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// `hivemux rename [--pane N | --window N] [--clear | <name>...]`
///
/// Without a target it names the pane it runs in, so an agent can label its
/// own pane.
pub fn rename(args: &[String]) -> Result<()> {
    let mut rest = args.to_vec();
    let clear = take_flag(&mut rest, "--clear");
    let workspace = take_window(&mut rest)?;
    let target = match workspace {
        Some(n) => RenameTarget::Workspace(n),
        None => RenameTarget::Pane(take_pane(&mut rest)?.context("which pane? use --pane N")?),
    };
    let name = if clear {
        None
    } else if rest.is_empty() {
        bail!("usage: hivemux rename [--pane N | --window N] [--clear | <name>...]");
    } else {
        Some(rest.join(" "))
    };
    client::request(Request::Rename { target, name })?;
    Ok(())
}

/// `hivemux new [--float] [--window N] [-- command args...]`
pub fn new(args: &[String]) -> Result<()> {
    let (options, command) = match args.iter().position(|a| a == "--") {
        Some(i) => (args[..i].to_vec(), args[i + 1..].to_vec()),
        None => (args.to_vec(), Vec::new()),
    };
    let mut options = options;
    let float = take_flag(&mut options, "--float");
    let workspace = take_window(&mut options)?;
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
        serde_json::json!([{ "matcher": "*", "hooks": [{
            "type": "command",
            "command": format!("hivemux status {state}"),
            "timeout": 10,
        }]}])
    };
    let config = serde_json::json!({
        "hooks": {
            "SessionStart": hook("idle"),
            "UserPromptSubmit": hook("working"),
            "PreToolUse": hook("working"),
            "PermissionRequest": hook("blocked"),
            "Stop": hook("idle"),
            "SessionEnd": hook("clear"),
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

/// `--window N`, or `--workspace N` as it was called before.
fn take_window(args: &mut Vec<String>) -> Result<Option<u8>> {
    let mut window = None;
    for option in ["--window", "--workspace"] {
        if let Some(n) = take_value(args, option)? {
            window = Some(
                n.parse::<u8>()
                    .with_context(|| format!("{option} takes a number from 1 to 9"))?,
            );
        }
    }
    Ok(window)
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

/// `hivemux rename-session NAME`: renames the session this command is
/// for, `-s` picks another.
pub fn rename_session(args: &[String]) -> Result<()> {
    let [name] = args else {
        bail!("usage: hivemux rename-session NAME");
    };
    client::request(Request::RenameSession { name: name.clone() })?;
    println!("renamed to {name}");
    Ok(())
}

/// `hivemux save [NAME]`: every running session's layout in one save
/// file, `last` without a name.
pub fn save(args: &[String]) -> Result<()> {
    let name = match args {
        [] => crate::persist::LAST,
        [name] => name.as_str(),
        _ => bail!("usage: hivemux save [NAME]"),
    };
    let hive = client::snapshot_all()?;
    if hive.sessions.is_empty() {
        bail!("no session is running");
    }
    let path = crate::persist::write_hive(name, &hive)?;
    let names: Vec<&str> = hive.sessions.keys().map(String::as_str).collect();
    println!(
        "hive {name}: saved {} to {}",
        names.join(", "),
        path.display()
    );
    Ok(())
}

/// `hivemux restore [NAME]`: starts the sessions of a save file that do
/// not run, `last` without a name. `--list` shows the save files.
pub fn restore(args: &[String]) -> Result<()> {
    match args {
        [flag] if flag == "--list" => {
            for name in crate::persist::saves() {
                let Ok(Some(hive)) = crate::persist::load_hive(&name) else {
                    continue;
                };
                let sessions: Vec<String> = hive
                    .sessions
                    .iter()
                    .map(|(session, saved)| {
                        let age = crate::persist::now().saturating_sub(saved.saved_at);
                        let age = crate::sidebar::age(std::time::Duration::from_secs(age));
                        format!("{session} ({age} ago)")
                    })
                    .collect();
                println!("{name:<12} {}", sessions.join(", "));
            }
            Ok(())
        }
        [] | [_] => {
            let name = args.first().map_or(crate::persist::LAST, String::as_str);
            let started = client::restore_hive(name)?;
            if started.is_empty() {
                println!("every session of {name} runs already");
            } else {
                println!(
                    "started {}; attach with `hivemux -s NAME` or switch with Ctrl+B S",
                    started.join(", ")
                );
            }
            Ok(())
        }
        _ => bail!("usage: hivemux restore [NAME | --list]"),
    }
}

/// `hivemux hive <ls | save | restore | rm | rename>`: hives, saved groups
/// of sessions. `save` and `restore` also work without `hive`.
pub fn hive(args: &[String]) -> Result<()> {
    const USAGE: &str =
        "usage: hivemux hive <ls | save [NAME] | restore [NAME] | rm NAME | rename OLD NEW>";
    match args {
        [] => bail!(USAGE),
        [cmd] if cmd == "ls" => restore(&["--list".to_owned()]),
        [cmd, rest @ ..] if cmd == "save" => save(rest),
        [cmd, rest @ ..] if cmd == "restore" => restore(rest),
        [cmd, name] if cmd == "rm" => {
            crate::persist::delete_hive(name)?;
            println!("hive {name} deleted");
            Ok(())
        }
        [cmd, old, name] if cmd == "rename" => {
            crate::persist::rename_hive(old, name)?;
            println!("hive {old} is now {name}");
            Ok(())
        }
        _ => bail!(USAGE),
    }
}

/// `hivemux update`: the running server becomes this binary, keeping every
/// pane and the attached client, see `upgrade`.
pub fn update() -> Result<()> {
    let exe = std::env::current_exe()?;
    let reply = client::request(Request::Upgrade { exe: exe.clone() })?;
    let panes = reply["panes"].as_u64().unwrap_or(0);
    // The new server takes the socket over; wait until it answers.
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if client::request(Request::List).is_ok() {
            println!("updated to {} ({panes} panes kept)", exe.display());
            return Ok(());
        }
    }
    anyhow::bail!("the server did not come back, see its log next to the socket")
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
    fn subagents_do_not_finish_the_agent() {
        let sub = serde_json::json!({"hook_event_name": "Stop", "agent_id": "a1"});
        assert!(ignore_hook(&sub, Some(AgentState::Idle)));
        assert!(!ignore_hook(&sub, Some(AgentState::Working)));
        let main = serde_json::json!({"hook_event_name": "Stop", "session_id": "s"});
        assert!(!ignore_hook(&main, Some(AgentState::Idle)));
        let stop = serde_json::json!({"hook_event_name": "SubagentStop"});
        assert!(ignore_hook(&stop, Some(AgentState::Working)));
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
