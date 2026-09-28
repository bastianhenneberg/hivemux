# hivemux 🐝

A terminal multiplexer and window manager for humans and their coding agents, written in Rust.

hivemux combines tiling and floating windows with a tmux-style prefix, and keeps an eye on
the agents running in its panes: working, blocked, or idle.

> **Status:** early prototype. hivemux runs shells in split panes, including full-screen
> programs like nvim, spread over workspaces, and keeps them running when you detach.

## Ideas

- **tmux-style prefix keys** (`Ctrl+B` by default)
- **Tiling and floating windows**: a BSP layout tree with a floating layer on top, plus workspaces
- **Server/client architecture**: detach and reattach without stopping work
- **Agent-aware**: every pane is marked working, blocked, or idle
- **Socket API** so agents and scripts can drive hivemux

Inspired by [TUIOS](https://github.com/Gaurav-Gosain/tuios) and [herdr](https://github.com/herdrdev/herdr).

## Roadmap

1. ~~One shell in one pane (PTY → VT emulation → rendering)~~ ✓
2. ~~Splits with a BSP tree, focus, resize, prefix keys~~ ✓
3. ~~Server/client split with detach and reattach~~ ✓
4. ~~Floating windows and workspaces~~ ✓
5. ~~Copy mode, scrollback, mouse~~ ✓
6. ~~Agent status, socket API~~ ✓
7. ~~Layout persistence across restarts~~ ✓

## Build

```bash
cargo build --release
./target/release/hivemux            # attach to the session, or start one
./target/release/hivemux attach     # attach, fail if there is no session
./target/release/hivemux kill-server  # stop the server and every shell in it
./target/release/hivemux keys         # list the key bindings
```

The first `hivemux` starts a server in the background. It owns the shells and keeps them
running after you detach or close the terminal. The socket lives in
`$XDG_RUNTIME_DIR/hivemux/`, set `HIVEMUX_SOCKET` to use another one. One client is attached at
a time, attaching from a second terminal detaches the first.

| Key | Action |
|---|---|
| `Ctrl+B` `%` | Split the pane side by side |
| `Ctrl+B` `"` | Split the pane top and bottom |
| `Ctrl+B` `x` | Close the pane (asks first) |
| `Ctrl+B` `r` | Name the pane (`Ctrl+B` `w` `r` names the workspace) |
| `Ctrl+B` `z` | Zoom: the pane alone over the whole workspace, again to go back |
| `Ctrl+B` `{` / `}` | Swap the pane with the previous / next one |
| `Ctrl+B` `=` | Give all panes equal sizes |
| `Ctrl+B` `f` … | Floating panes menu: `f` float/tile, `n` new, `o` next, arrows move |
| `Ctrl+B` `Shift+←↑↓→` | Move a floating pane |
| `Ctrl+B` `w` … | Workspaces menu: `1`–`9`, `c` new, `n`/`p` next/previous |
| `Ctrl+B` `1`–`9` | Go to workspace, an empty one starts a shell |
| `Ctrl+B` `←↑↓→` | Focus the pane in that direction |
| `Ctrl+B` `o` | Focus the next pane |
| `Ctrl+B` `;` | Back to the pane focused before, across workspaces |
| `Ctrl+B` `g` | Go to any pane: type to filter by workspace, name, program, state or directory |
| `Ctrl+B` `a` | Jump to the agent that has waited longest for you, then to finished ones |
| `Ctrl+B` `[` | Copy mode: scroll back with vim keys (`Ctrl-U`/`Ctrl-D`, `Ctrl-B`/`Ctrl-F`, `g`/`G`), `/` and `?` search down and up, `n`/`N` next, `v` select, `y` copy, `q` quit |
| `Ctrl+B` `u` | Copy mode, one page up right away (`PgUp` works too) |
| `Ctrl+B` `E` | The pane's whole history in `$EDITOR` |
| `Ctrl+B` `c` … | Your commands from the config, e.g. `g` for lazygit |
| `Ctrl+B` `Ctrl+←↑↓→` | Resize by one cell (`Alt` for five), floating panes too |
| `Ctrl+B` `q` | Quit menu: detach, close the pane or end the session |
| `Ctrl+B` `d` | Detach, the shells keep running |
| `Ctrl+B` `Q` | Quit: end the session and every shell in it (asks first) |
| `Ctrl+B` `b` | Show or hide the sidebar |
| `Ctrl+B` `e` | Into the sidebar: `j`/`k` move, `Enter` go, `r` name, `x` close, `Esc` back |
| `Ctrl+B` `,` | Settings |
| `Ctrl+B` `R` | Reload the config file |
| `Ctrl+B` `?` | Show all keys |
| `Ctrl+B` `Ctrl+B` | Send `Ctrl+B` to the shell |

Pressing `Ctrl+B` opens a menu with every key, like which-key in Neovim. `f` and `w` open
submenus, `⌫` goes back.
Focus, move and resize repeat: for 600 ms after one of them, arrow keys work without the prefix.
A pane closes when its shell exits, hivemux exits with the last pane.

## Sidebar

A sidebar on the right, like the rail in TUIOS, lists every workspace with its project (the
focused pane's directory) and what its agents are doing, every agent across all workspaces with
its workspace, directory and how long it has been in its state, and the git branch of the
focused pane with its changed files. Agents waiting for you come first. Enter or a click on a
changed file shows its diff in a floating pane, `o` in the sidebar opens it in `$EDITOR`. Click a row to go there. `Ctrl+B b` shows or
hides it (as in TUIOS), `Ctrl+B e` moves the keyboard into it, the settings put it left or right.

## Agents

Every pane running a coding agent shows what it is doing: `● working`, `◆ blocked` (it waits
for you), `✦ done` (finished while you were elsewhere) or `✓ idle`. A workspace tab turns red when an agent there waits, the bell rings, and
`Ctrl+B a` takes you to it. You also get a desktop notification when an agent waits or finishes
while you look elsewhere or are detached: through `notify-send`, or as OSC 9 for terminals like
Ghostty and WezTerm, or not at all (setting `notifications = "system" | "terminal" | "off"`).

hivemux recognises claude, codex, opencode, aider, gemini, crush, goose, amp, cursor-agent, qwen
and kilo. It guesses from their output and from questions on the screen. For exact states, let
the agent report them with hooks. For Claude Code:

```bash
cargo install --path .     # puts hivemux on your PATH
hivemux hooks              # prints the hooks for ~/.claude/settings.json
```

The hooks call `hivemux status working|blocked|idle|clear`, which knows its pane from
`$HIVEMUX_PANE` and does nothing outside hivemux. It reads the hook's JSON from stdin: a subagent
finishing does not mark the agent done, and the session id is kept to resume the agent later.

Scripts and agents can drive hivemux too:

```bash
hivemux list [--json]                          # panes with program, state and directory
hivemux send --pane 3 "run the tests"          # type into a pane, then Enter
hivemux new --workspace 2 -- claude --resume   # start a command in a new pane
hivemux new --float                            # a floating shell
hivemux rename "tests"                          # name the pane this runs in
hivemux wait --pane 3 --until ready            # wait until an agent is idle or done
hivemux read --pane 3 --lines 20               # what it wrote last
hivemux rename --workspace 2 api               # name a workspace
```

## Restarts

hivemux saves the layout to `~/.local/state/hivemux/session.json` while it runs: workspaces,
splits, floating panes and each pane's directory. If the server dies with the machine, the next
`hivemux` brings the layout back, shells start in their old directories, and Claude Code and
Codex sessions known from the hooks are resumed (`claude --resume <id>`). Processes and screen
contents cannot survive a restart. Ending the session on purpose (`Ctrl+B Q`, `kill-server`, or
exiting the last shell) deletes the file.

## Mouse

The terminal window's title shows the workspace and the focused pane, e.g. `hivemux · 2 api · claude`.


- Click a pane to focus it, click a workspace tab to switch to it.
- Drag the border between two panes to resize them.
- Drag a floating pane by its title bar, resize it by its bottom right corner.
- Drag over text to select it, it is copied when you let go. A double click copies a word.
- The wheel scrolls back through the history, typing jumps back to the live screen.
- Programs that use the mouse themselves, like nvim or htop, get the events. Hold Shift to
  select text in them anyway.

Copying uses OSC 52, so it works over SSH as well. Your terminal has to allow it (kitty,
Alacritty, Ghostty, foot and WezTerm do).

## Settings

`Ctrl+B` `,` opens the settings. Changes apply right away and are saved to
`~/.config/hivemux/config.toml` (or `$XDG_CONFIG_HOME/hivemux/`, or `$HIVEMUX_CONFIG`).
The file can also be edited by hand, hivemux reads it when the server starts:

```toml
theme = "omarchy"   # "hivemux", "omarchy" to follow the desktop, or an Omarchy theme name

[which_key]
enabled = true      # show the key menu after Ctrl+B
position = "left"   # "left" or "right" bottom corner

[sidebar]
enabled = true
side = "right"      # "left" or "right"

[bars]
control = "bottom"  # hivemux badge, mode and hints: "top" or "bottom"
tabs = "bottom"     # workspace tabs: "top", "bottom" or "off"
path = "bottom"     # directory of the focused pane: "top", "bottom" or "off"
```

Themes: `hivemux` is honey on your terminal's colours. `omarchy` follows the current Omarchy theme
and changes with it. Any installed Omarchy theme (`catppuccin`, `gruvbox`, `tokyo-night`, …) can be
picked by name, its `colors.toml` provides the colours. In the settings, ←/→ step through them.

Your own commands go in the `Ctrl+B c` menu, run with `sh -c` in the focused pane's directory,
in a floating pane unless `float = false`:

```toml
[[commands]]
key = "g"
name = "lazygit"
command = "lazygit"

[[commands]]
key = "t"
name = "tests"
command = "cargo test"
float = false
```

Elements on the same side share one line. New panes start in the directory of the focused
pane.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
