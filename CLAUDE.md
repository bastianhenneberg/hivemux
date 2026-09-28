# hivemux

Terminal-Multiplexer und Fenster-Manager in Rust. Ideen aus TUIOS (Floating, BSP-Tiling,
Workspaces) und herdr (Agent-Status, Server/Client, Socket-API), aber eigener Code.

## AI Brain

Projekt-Slug: `hivemux`

## Stack

- UI: `ratatui` (crossterm über `ratatui::crossterm`, keine eigene crossterm-Abhängigkeit,
  damit die Versionen nicht auseinanderlaufen)
- PTY: `portable-pty`
- VT-Emulation: `vt100` (später evtl. `alacritty_terminal`)

## Befehle

```bash
cargo run            # starten, Ctrl+B q beendet
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo test
```

## Konventionen

- Lizenz: MIT OR Apache-2.0
- Code und Kommentare auf Englisch
