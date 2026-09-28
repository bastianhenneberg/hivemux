# hivemux 🐝

A terminal multiplexer and window manager for humans and their coding agents, written in Rust.

hivemux combines tiling and floating windows with a tmux-style prefix, and keeps an eye on
the agents running in its panes: working, blocked, or idle.

> **Status:** early prototype. hivemux runs shells in split panes, including full-screen
> programs like nvim, and keeps them running when you detach.

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
4. Floating windows and workspaces
5. Copy mode, scrollback, mouse
6. Agent status, socket API
7. Layout persistence across restarts

## Build

```bash
cargo build --release
./target/release/hivemux            # attach to the session, or start one
./target/release/hivemux attach     # attach, fail if there is no session
./target/release/hivemux kill-server
```

The first `hivemux` starts a server in the background. It owns the shells and keeps them
running after you detach or close the terminal. The socket lives in
`$XDG_RUNTIME_DIR/hivemux/`, set `HIVEMUX_SOCKET` to use another one. One client is attached at
a time, attaching from a second terminal detaches the first.

| Key | Action |
|---|---|
| `Ctrl+B` `%` | Split the pane side by side |
| `Ctrl+B` `"` | Split the pane top and bottom |
| `Ctrl+B` `x` | Close the pane |
| `Ctrl+B` `←↑↓→` | Focus the pane in that direction |
| `Ctrl+B` `o` | Focus the next pane |
| `Ctrl+B` `Ctrl+←↑↓→` | Resize by one cell (`Alt` for five) |
| `Ctrl+B` `d` | Detach, the shells keep running |
| `Ctrl+B` `q` | Quit, closing every pane |
| `Ctrl+B` `Ctrl+B` | Send `Ctrl+B` to the shell |

Focus and resize repeat: for 600 ms after one of them, arrow keys work without the prefix.
A pane closes when its shell exits, hivemux exits with the last pane.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
