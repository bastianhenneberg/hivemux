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
- Workspace-Logik (Tiling-Baum + Floats + Fokus) ist reine Geometrie in `src/workspace.rs` und dort
  getestet. `App` hält den aktiven Workspace in `ws`, die übrigen in `hidden`, und tauscht beim Wechsel.
- Maus, Selektion, Copy-Mode: `App::mouse`, `App::copy_key`. Positionen in Selektion/Copy-Cursor sind
  *absolute* Zeilen (0 = älteste Zeile im Verlauf), damit Scrollen sie nicht verschiebt. Text holt
  `pane::text_between`. Maus-Kodierung für Programme: `src/mouse.rs`, Clipboard (OSC 52): `src/clipboard.rs`.
  tmux-Falle 2: Wird die letzte tmux-Session beendet, beendet sich der tmux-Server und vergisst
  `set-clipboard on` – vor OSC-52-Tests jedes Mal neu setzen.
  tmux-Falle: `;` in `send-keys` ist ein Befehlstrenner, auch mit `-l` – als `'\;'` schicken.
  Headless testen: rohe SGR-Sequenzen per `tmux send-keys -l $'\e[<0;x;yM'`; OSC 52 landet mit
  `set-clipboard on` im tmux-Buffer (`tmux show-buffer`).
- Agent-Status: `src/agent.rs` (Erkennung per argv aus `/proc/<pgid>/cmdline`, Aktivität, Fragen
  auf dem Screen; gemeldeter Status aus Hooks hat Vorrang). `App::update_agents` läuft bei jedem
  Tick (500 ms) und klingelt bei neuem `blocked`. Beantwortete Fragen: `Pane::note_input`.
- Verbindungen attachen erst nach `ClientMsg::Attach`; alle anderen (CLI, Agents) schicken
  `ClientMsg::Request` und bekommen `ServerMsg::Reply`. Befehle: `src/cli.rs`.
- Agent-Tests ohne echten Agent: Skript namens `claude` im Scratchpad, das arbeitet und dann eine
  Frage stellt, per `hivemux new --workspace 2 -- <skript>` starten.
- Persistenz: `src/persist.rs`, Datei `~/.local/state/hivemux/session.json` (mit `HIVEMUX_SOCKET`
  daneben als `<socket>.state.json`, mit `HIVEMUX_STATE` frei wählbar). Gespeichert höchstens 1×/s
  bei Änderung, gelöscht am Ende von `App::run` (nur bewusstes Beenden kommt dort an). Test: Server
  mit `kill -9` töten und neu starten.
- Farben nie fest verdrahten: `theme::current()` (accent, on_accent, subtle, danger, success,
  warning). Themes aus Omarchys `colors.toml` (`~/.config/omarchy/themes`, `~/.local/share/omarchy/themes`,
  aktuell: `~/.local/state/omarchy/current/`), `src/theme.rs`. Test des Live-Folgens mit falschem
  `HOME` samt nachgebautem `.local/state/omarchy/current`.
- Bilder (kitty graphics): `src/graphics.rs` (Scanner, Command, PaneGraphics), im Pane-Reader
  vor vt100 abgefangen, `App::image_output` platziert nach jedem Frame. Test ohne Grafik-Terminal:
  Client unter `script -qfc 'env HIVEMUX_GRAPHICS=1 hivemux' raw.txt` in tmux starten und die
  APC-Sequenzen in `raw.txt` prüfen; Testprogramm mit echter PNG über Python (zlib/struct).
- Tastenbelegung nach dem Prefix steht **nur** in `src/bindings.rs` (`BINDINGS`). Which-Key-Menü,
  Hilfe-Overlay und `hivemux keys` lesen daraus, neue Befehle dort eintragen, nicht in `app.rs`.
- Einstellungen: `src/config.rs` (serde/TOML, `SETTINGS`-Tabelle fürs Settings-Menü). Neue
  Einstellung = Feld in `Config` + Eintrag in `SETTINGS`, das Menü liest nur die Tabelle.
- Manuell testen ohne die eigene Session zu stören: `HIVEMUX_SOCKET=/tmp/x.sock` und `HIVEMUX_CONFIG=/tmp/x.toml`
  setzen (sonst wird die echte Config überschrieben), headless in tmux starten (`tmux new-session -d ...`, `send-keys`, `capture-pane -p`).

## Befehle

```bash
cargo run            # starten, Ctrl+B q Quit-Menü, Ctrl+B d detacht, `cargo run -- kill-server` beendet
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo test
```

## Konventionen

- Lizenz: MIT OR Apache-2.0
- Code und Kommentare auf Englisch
