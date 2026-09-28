# hivemux

Terminal-Multiplexer und Fenster-Manager in Rust. Ideen aus TUIOS (Floating, BSP-Tiling,
Workspaces) und herdr (Agent-Status, Server/Client, Socket-API), aber eigener Code.

## AI Brain

Projekt-Slug: `hivemux`

## Stack

- UI: `ratatui`. `crossterm` ist zusätzlich direkt eingebunden, nur wegen des `serde`-Features
  (Events gehen per Socket vom Client zum Server). Version muss zu der von ratatui passen,
  sonst gibt es zwei crossterm-Versionen: `cargo tree -d | grep crossterm` prüft das.
- PTY: `portable-pty`
- VT-Emulation: `vt100` (später evtl. `alacritty_terminal`)

## Architektur

- `hivemux server` (vom ersten Client im Hintergrund per `setsid` gestartet) besitzt PTYs,
  Layout und rendert. Der Client ist dünn: Raw-Mode, schickt crossterm-Events, schreibt die
  ANSI-Ausgabe des Servers auf stdout. Protokoll: `src/protocol.rs` (Tag + Länge + Payload).
- Der Server rendert mit `Viewport::Fixed` in einen Socket-Writer. **Nie** `terminal.clear()`
  oder `terminal.resize()` aufrufen: beide fragen das (nicht vorhandene) TTY des Servers
  nach Cursor bzw. Größe. Bei Resize wird das Terminal neu gebaut.
- Tastenbelegung nach dem Prefix steht **nur** in `src/bindings.rs` (`BINDINGS`). Which-Key-Menü,
  Hilfe-Overlay und `hivemux keys` lesen daraus, neue Befehle dort eintragen, nicht in `app.rs`.
- Manuell testen ohne die eigene Session zu stören: `HIVEMUX_SOCKET=/tmp/x.sock` setzen,
  headless in tmux starten (`tmux new-session -d ...`, `send-keys`, `capture-pane -p`).

## Befehle

```bash
cargo run            # starten, Ctrl+B q detacht, `cargo run -- kill-server` beendet
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo test
```

## Konventionen

- Lizenz: MIT OR Apache-2.0
- Code und Kommentare auf Englisch
