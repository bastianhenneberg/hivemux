//! The kitty graphics protocol, for images in panes: finding the commands
//! in a pane's output, reading what they say, and rewriting them for the
//! client's terminal.
//!
//! A command is an APC string: `ESC _ G <key=value,...> ; <base64> ESC \`.
//! vt100 drops these, so the pane's reader takes them out of the stream
//! first, noting where the cursor was.

use std::collections::HashMap;

/// One piece of a pane's output: plain terminal output, or the body of a
/// kitty graphics command (what is between `ESC _ G` and `ESC \`).
#[derive(Debug, PartialEq, Eq)]
pub enum Chunk {
    Text(Vec<u8>),
    Graphics(Vec<u8>),
}

/// Splits output into text and graphics commands. Commands may arrive in
/// pieces across reads, the scanner keeps what it has seen of one.
#[derive(Default)]
pub struct Scanner {
    /// Bytes of a command started but not finished, `ESC _ G` included.
    pending: Vec<u8>,
    in_command: bool,
}

/// Commands longer than this are dropped, so a broken stream cannot eat
/// all memory.
const MAX_COMMAND: usize = 64 * 1024 * 1024;

impl Scanner {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Chunk> {
        let mut out = Vec::new();
        let mut text = Vec::new();
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut i = 0;
        while i < data.len() {
            if self.in_command {
                // The body runs to the string terminator `ESC \`.
                match find(&data[i..], b"\x1b\\") {
                    Some(end) => {
                        let body = data[i..i + end].to_vec();
                        if body.len() <= MAX_COMMAND {
                            out.push(Chunk::Graphics(body));
                        }
                        self.in_command = false;
                        i += end + 2;
                    }
                    None => {
                        // Keep the rest for the next read, including a lone
                        // ESC at the end that may start the terminator.
                        if data.len() - i <= MAX_COMMAND {
                            self.pending = data[i..].to_vec();
                        } else {
                            self.in_command = false;
                        }
                        break;
                    }
                }
                continue;
            }
            match find(&data[i..], b"\x1b_G") {
                Some(start) => {
                    text.extend_from_slice(&data[i..i + start]);
                    if !text.is_empty() {
                        out.push(Chunk::Text(std::mem::take(&mut text)));
                    }
                    self.in_command = true;
                    i += start + 3;
                }
                None => {
                    // A command may start across the read boundary: hold
                    // back a trailing `ESC` or `ESC _`.
                    let rest = &data[i..];
                    let keep = [b"\x1b_".as_slice(), b"\x1b"]
                        .iter()
                        .find(|p| rest.ends_with(p))
                        .map_or(0, |p| p.len());
                    text.extend_from_slice(&rest[..rest.len() - keep]);
                    self.pending = rest[rest.len() - keep..].to_vec();
                    break;
                }
            }
        }
        if !text.is_empty() {
            out.push(Chunk::Text(text));
        }
        out
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A parsed graphics command: its keys and its base64 payload.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Command {
    pub keys: HashMap<String, String>,
    pub payload: Vec<u8>,
}

impl Command {
    pub fn parse(body: &[u8]) -> Command {
        let (control, payload) = match body.iter().position(|&b| b == b';') {
            Some(i) => (&body[..i], body[i + 1..].to_vec()),
            None => (body, Vec::new()),
        };
        let keys = String::from_utf8_lossy(control)
            .split(',')
            .filter_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                Some((key.to_owned(), value.to_owned()))
            })
            .collect();
        Command { keys, payload }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.keys.get(key).map(String::as_str)
    }

    pub fn number(&self, key: &str) -> Option<u32> {
        self.get(key)?.parse().ok()
    }

    /// The action, `t` (transmit) when the command does not say.
    pub fn action(&self) -> &str {
        self.get("a").unwrap_or("t")
    }

    /// Whether more chunks of this transmission follow.
    pub fn more(&self) -> bool {
        self.get("m") == Some("1")
    }

    /// The command again, with the given keys replaced or added and the
    /// ones set to `None` removed, as a complete APC string.
    pub fn encode(&self, changes: &[(&str, Option<String>)]) -> Vec<u8> {
        let mut keys = self.keys.clone();
        for (key, value) in changes {
            match value {
                Some(value) => keys.insert((*key).to_owned(), value.clone()),
                None => keys.remove(*key),
            };
        }
        let mut sorted: Vec<(String, String)> = keys.into_iter().collect();
        sorted.sort();
        let control = sorted
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut out = b"\x1b_G".to_vec();
        out.extend_from_slice(control.as_bytes());
        if !self.payload.is_empty() {
            out.push(b';');
            out.extend_from_slice(&self.payload);
        }
        out.extend_from_slice(b"\x1b\\");
        out
    }
}

/// The reply the terminal gives to command `cmd`: `OK`, addressed by its
/// image id or number, or nothing when it asked for none (`q=1`/`q=2`).
pub fn reply(cmd: &Command, message: &str) -> Option<Vec<u8>> {
    let quiet = cmd.number("q").unwrap_or(0);
    if quiet >= 2 || (quiet == 1 && message == "OK") {
        return None;
    }
    let mut keys = Vec::new();
    if let Some(i) = cmd.get("i") {
        keys.push(format!("i={i}"));
    }
    if let Some(n) = cmd.get("I") {
        keys.push(format!("I={n}"));
    }
    if keys.is_empty() {
        return None;
    }
    Some(format!("\x1b_G{};{message}\x1b\\", keys.join(",")).into_bytes())
}

/// The size in pixels of an image from the start of its payload: from the
/// `s`/`v` keys for raw pixels, from the PNG header for `f=100`.
pub fn pixel_size(cmd: &Command) -> Option<(u32, u32)> {
    if let (Some(w), Some(h)) = (cmd.number("s"), cmd.number("v")) {
        return Some((w, h));
    }
    if cmd.get("f") != Some("100") {
        return None;
    }
    // A PNG starts with an 8 byte signature and the IHDR chunk, whose
    // width and height are the big-endian u32s at bytes 16 and 20. 32
    // base64 characters decode to the first 24 bytes.
    let head = decode_base64(cmd.payload.get(..32)?)?;
    if head.get(..8)? != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = u32::from_be_bytes(head.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(head.get(20..24)?.try_into().ok()?);
    Some((width, height))
}

/// How many cells an image covers: the `c`/`r` keys, or its pixel size
/// over the cell size.
pub fn cell_size(
    cmd: &Command,
    pixels: Option<(u32, u32)>,
    cell: (u16, u16),
) -> Option<(u16, u16)> {
    let (cw, ch) = (u32::from(cell.0.max(1)), u32::from(cell.1.max(1)));
    let from_pixels = pixels.map(|(w, h)| (w.div_ceil(cw), h.div_ceil(ch)));
    let cols = cmd.number("c").or(from_pixels.map(|p| p.0))?;
    let rows = cmd.number("r").or(from_pixels.map(|p| p.1))?;
    Some((
        cols.min(u32::from(u16::MAX)) as u16,
        rows.min(u32::from(u16::MAX)) as u16,
    ))
}

fn decode_base64(text: &[u8]) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::new();
    for chunk in text.chunks(4) {
        let digits: Vec<u32> = chunk
            .iter()
            .take_while(|&&c| c != b'=')
            .map(|&c| value(c))
            .collect::<Option<_>>()?;
        let n = digits.iter().fold(0, |acc, d| acc << 6 | d) << (6 * (4 - digits.len()));
        for i in 0..digits.len().saturating_sub(1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

/// Image ids on the client's terminal are unique across all panes: every
/// image a program sends gets one of these.
static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

fn next_id() -> u32 {
    NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Where an image is shown in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// The image on the client's terminal.
    pub image: u32,
    /// The placement id, 1 unless the program chose one.
    pub id: u32,
    /// Absolute row (0 = oldest line of history) and column of the top
    /// left cell.
    pub row: usize,
    pub col: u16,
    pub cols: u16,
    pub rows: u16,
}

/// Transmitted images kept to send again to a client that attaches later.
const KEEP_IMAGES: usize = 32;

/// A pane's images, shared between its reader thread, which handles the
/// program's commands, and the server's main thread, which draws them.
#[derive(Default)]
pub struct PaneGraphics {
    /// Whether the attached client shows images. Without that, commands
    /// are dropped, as vt100 would.
    pub enabled: bool,
    /// The size of a cell in pixels on the client's terminal.
    pub cell: (u16, u16),
    /// The program's image ids and numbers mapped to the client's ids.
    ids: HashMap<u32, u32>,
    /// A transmission in several chunks: the image, its first chunk and
    /// whether to show it once complete.
    pending: Option<(u32, Command, bool)>,
    pub placements: Vec<Placement>,
    /// Commands for the client's terminal, sent with the next frame.
    pub outbox: Vec<Vec<u8>>,
    /// Answers for the program, written to its pty by the main thread.
    pub replies: Vec<Vec<u8>>,
    /// What was sent for each image, oldest first, to send it again.
    sent: Vec<(u32, Vec<Vec<u8>>)>,
}

/// What handling a command did to the cursor: move it past an image.
pub struct Advance {
    pub cols: u16,
    pub rows: u16,
}

impl PaneGraphics {
    /// Handles one command from the program. `cursor` is where the cursor
    /// is, as (absolute row, column). Returns how far the cursor moves past
    /// an image that was shown there.
    pub fn handle(&mut self, body: &[u8], cursor: (usize, u16)) -> Option<Advance> {
        if !self.enabled {
            return None;
        }
        let cmd = Command::parse(body);

        // A further chunk of a transmission carries only `m` and data.
        if !cmd.keys.contains_key("a") && !cmd.keys.contains_key("i") && self.pending.is_some() {
            let (image, first, show) = self.pending.take().expect("checked above");
            self.forward(image, cmd.encode(&[]));
            if cmd.more() {
                self.pending = Some((image, first, show));
                return None;
            }
            if let Some(reply) = reply(&first, "OK") {
                self.replies.push(reply);
            }
            return show
                .then(|| self.place(image, &first, &first, cursor))
                .flatten();
        }

        match cmd.action() {
            // "Can you show images?" Yes.
            "q" => {
                if let Some(reply) = reply(&cmd, "OK") {
                    self.replies.push(reply);
                }
                None
            }
            action @ ("t" | "T") => {
                let image = self.image_for(&cmd);
                // Only data goes to the client now; where to show it is
                // worked out when drawing.
                let transmit = cmd.encode(&[
                    ("a", Some("t".into())),
                    ("i", Some(image.to_string())),
                    ("I", None),
                    ("p", None),
                    ("q", Some("2".into())),
                ]);
                self.start_image(image);
                self.forward(image, transmit);
                let show = action == "T";
                if cmd.more() {
                    self.pending = Some((image, cmd, show));
                    return None;
                }
                if let Some(reply) = reply(&cmd, "OK") {
                    self.replies.push(reply);
                }
                show.then(|| self.place(image, &cmd, &cmd, cursor))
                    .flatten()
            }
            "p" => {
                let id = cmd.number("i").or(cmd.number("I"))?;
                let Some(&image) = self.ids.get(&id) else {
                    if let Some(reply) = reply(&cmd, "ENOENT:no such image") {
                        self.replies.push(reply);
                    }
                    return None;
                };
                // The size is known from when the image was sent.
                let first = self.first_command(image).unwrap_or_default();
                if let Some(reply) = reply(&cmd, "OK") {
                    self.replies.push(reply);
                }
                self.place(image, &cmd, &first, cursor)
            }
            "d" => {
                self.delete(&cmd);
                None
            }
            _ => None,
        }
    }

    /// The client's id for the program's image, a new one for a new image
    /// or one without an id.
    fn image_for(&mut self, cmd: &Command) -> u32 {
        match cmd.number("i").or(cmd.number("I").map(|n| n | 1 << 31)) {
            Some(id) => *self.ids.entry(id).or_insert_with(next_id),
            None => next_id(),
        }
    }

    /// Starts a new record of what is sent for `image`, replacing an older
    /// one and forgetting the oldest images beyond the limit.
    fn start_image(&mut self, image: u32) {
        self.sent.retain(|(id, _)| *id != image);
        self.sent.push((image, Vec::new()));
        if self.sent.len() > KEEP_IMAGES {
            self.sent.remove(0);
        }
    }

    fn forward(&mut self, image: u32, command: Vec<u8>) {
        if let Some((_, chunks)) = self.sent.iter_mut().find(|(id, _)| *id == image) {
            chunks.push(command.clone());
        }
        self.outbox.push(command);
    }

    /// The first command sent for `image`, which has its format and size.
    fn first_command(&self, image: u32) -> Option<Command> {
        let (_, chunks) = self.sent.iter().find(|(id, _)| *id == image)?;
        let first = chunks.first()?;
        Some(Command::parse(
            first.strip_prefix(b"\x1b_G")?.strip_suffix(b"\x1b\\")?,
        ))
    }

    /// Shows `image` at the cursor, sized from `place` or else from the
    /// image data in `data`. Returns how far the cursor moves past it.
    fn place(
        &mut self,
        image: u32,
        place: &Command,
        data: &Command,
        cursor: (usize, u16),
    ) -> Option<Advance> {
        let pixels = pixel_size(data);
        let size_from = if place.get("c").is_some() || place.get("r").is_some() {
            place
        } else {
            data
        };
        let (cols, rows) = cell_size(size_from, pixels, self.cell)?;
        let id = place.number("p").unwrap_or(1);
        self.placements
            .retain(|p| !(p.image == image && p.id == id));
        self.placements.push(Placement {
            image,
            id,
            row: cursor.0,
            col: cursor.1,
            cols,
            rows,
        });
        (place.get("C") != Some("1")).then_some(Advance { cols, rows })
    }

    /// `a=d`: removes placements, by image or all. Capital letters free the
    /// image data too.
    fn delete(&mut self, cmd: &Command) {
        let what = cmd.get("d").unwrap_or("a");
        match what {
            "i" | "I" => {
                let Some(id) = cmd.number("i") else { return };
                let Some(&image) = self.ids.get(&id) else {
                    return;
                };
                let only = cmd.number("p");
                self.placements
                    .retain(|p| p.image != image || only.is_some_and(|id| p.id != id));
                if what == "I" {
                    self.forget(image);
                }
            }
            "n" | "N" => {
                let Some(n) = cmd.number("I") else { return };
                let Some(&image) = self.ids.get(&(n | 1 << 31)) else {
                    return;
                };
                self.placements.retain(|p| p.image != image);
                if what == "N" {
                    self.forget(image);
                }
            }
            // Everything else, all placements and by position alike, clears
            // them all: close enough for what programs use.
            _ => {
                let images: Vec<u32> = self.placements.iter().map(|p| p.image).collect();
                self.placements.clear();
                if what.chars().all(|c| c.is_ascii_uppercase()) {
                    for image in images {
                        self.forget(image);
                    }
                }
            }
        }
    }

    fn forget(&mut self, image: u32) {
        self.ids.retain(|_, id| *id != image);
        self.sent.retain(|(id, _)| *id != image);
        self.outbox
            .push(format!("\x1b_Ga=d,d=I,i={image},q=2\x1b\\").into_bytes());
    }

    /// Removes placements on the screen when it is cleared. `history` is the
    /// number of lines above the screen, `all` includes the history.
    pub fn clear_screen(&mut self, history: usize, all: bool) {
        self.placements.retain(|p| !all && p.row < history);
    }

    /// Everything sent so far, to bring a newly attached client up to date.
    pub fn resend(&mut self) {
        let chunks: Vec<Vec<u8>> = self.sent.iter().flat_map(|(_, c)| c.clone()).collect();
        self.outbox.extend(chunks);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_splits_text_and_commands() {
        let mut s = Scanner::default();
        let out = s.feed(b"hi\x1b_Ga=T,f=100;QUJD\x1b\\there");
        assert_eq!(
            out,
            vec![
                Chunk::Text(b"hi".to_vec()),
                Chunk::Graphics(b"a=T,f=100;QUJD".to_vec()),
                Chunk::Text(b"there".to_vec()),
            ]
        );
    }

    #[test]
    fn scanner_joins_commands_split_across_reads() {
        let mut s = Scanner::default();
        let mut out = s.feed(b"x\x1b");
        out.extend(s.feed(b"_Ga=q"));
        out.extend(s.feed(b",i=1;AAAA\x1b"));
        out.extend(s.feed(b"\\y"));
        assert_eq!(
            out,
            vec![
                Chunk::Text(b"x".to_vec()),
                Chunk::Graphics(b"a=q,i=1;AAAA".to_vec()),
                Chunk::Text(b"y".to_vec()),
            ]
        );
    }

    #[test]
    fn other_escapes_pass_through() {
        let mut s = Scanner::default();
        assert_eq!(
            s.feed(b"\x1b[31mred\x1b_Xother\x1b\\"),
            vec![Chunk::Text(b"\x1b[31mred\x1b_Xother\x1b\\".to_vec())]
        );
    }

    #[test]
    fn commands_parse_and_encode() {
        let cmd = Command::parse(b"a=T,i=7,f=100,m=1;AAAA");
        assert_eq!(cmd.action(), "T");
        assert_eq!(cmd.number("i"), Some(7));
        assert!(cmd.more());
        let encoded = cmd.encode(&[
            ("i", Some("42".into())),
            ("m", None),
            ("q", Some("2".into())),
        ]);
        assert_eq!(encoded, b"\x1b_Ga=T,f=100,i=42,q=2;AAAA\x1b\\");
        assert_eq!(Command::parse(b"m=0;BBBB").action(), "t");
    }

    #[test]
    fn replies_follow_the_quiet_key() {
        let query = Command::parse(b"a=q,i=31,s=1,v=1,f=24;AAAA");
        assert_eq!(reply(&query, "OK").unwrap(), b"\x1b_Gi=31;OK\x1b\\");
        assert_eq!(reply(&Command::parse(b"a=q,i=1,q=2"), "OK"), None);
        assert_eq!(reply(&Command::parse(b"a=q,i=1,q=1"), "OK"), None);
        assert!(reply(&Command::parse(b"a=q,i=1,q=1"), "ENOENT").is_some());
        assert_eq!(reply(&Command::parse(b"a=q"), "OK"), None);
    }

    fn enabled() -> PaneGraphics {
        PaneGraphics {
            enabled: true,
            cell: (10, 20),
            ..Default::default()
        }
    }

    #[test]
    fn disabled_panes_drop_commands() {
        let mut g = PaneGraphics::default();
        assert!(g.handle(b"a=q,i=1", (0, 0)).is_none());
        assert!(g.replies.is_empty() && g.outbox.is_empty());
    }

    #[test]
    fn queries_get_an_ok() {
        let mut g = enabled();
        g.handle(b"a=q,i=31,s=1,v=1,f=24;AAAA", (0, 0));
        assert_eq!(g.replies, vec![b"\x1b_Gi=31;OK\x1b\\".to_vec()]);
    }

    #[test]
    fn transmit_and_show_places_the_image_and_moves_the_cursor() {
        let mut g = enabled();
        let advance = g.handle(b"a=T,f=24,s=40,v=40,i=5;AAAA", (12, 3)).unwrap();
        assert_eq!((advance.cols, advance.rows), (4, 2));
        let image = g.placements[0].image;
        assert_eq!(
            g.placements[0],
            Placement {
                image,
                id: 1,
                row: 12,
                col: 3,
                cols: 4,
                rows: 2
            }
        );
        // The data goes out as a plain transmission with the client's id.
        let out = String::from_utf8(g.outbox[0].clone()).unwrap();
        assert!(out.contains("a=t") && out.contains(&format!("i={image}")) && out.contains("q=2"));
        // Shown again later by the program's id: the same image.
        g.handle(b"a=p,i=5,p=2,C=1", (30, 0));
        assert_eq!(g.placements.len(), 2);
        assert_eq!(g.placements[1].image, image);
        assert_eq!((g.placements[1].cols, g.placements[1].rows), (4, 2));
    }

    #[test]
    fn chunked_transmissions_show_once_complete() {
        let mut g = enabled();
        assert!(g.handle(b"a=T,f=24,s=20,v=20,m=1;AAAA", (0, 0)).is_none());
        assert!(g.placements.is_empty());
        assert!(g.handle(b"m=1;BBBB", (0, 0)).is_none());
        assert!(g.handle(b"m=0;CCCC", (0, 0)).is_some());
        assert_eq!(g.placements.len(), 1);
        assert_eq!(g.outbox.len(), 3);
    }

    #[test]
    fn delete_removes_placements_and_frees_images() {
        let mut g = enabled();
        g.handle(b"a=T,f=24,s=10,v=10,i=1;AAAA", (0, 0));
        g.handle(b"a=T,f=24,s=10,v=10,i=2;AAAA", (1, 0));
        g.handle(b"a=d,d=i,i=1", (0, 0));
        assert_eq!(g.placements.len(), 1);
        g.handle(b"a=d,d=A", (0, 0));
        assert!(g.placements.is_empty());
        assert!(String::from_utf8_lossy(g.outbox.last().unwrap()).contains("d=I"));
    }

    #[test]
    fn resend_repeats_the_transmissions() {
        let mut g = enabled();
        g.handle(b"a=T,f=24,s=10,v=10,i=1;AAAA", (0, 0));
        g.outbox.clear();
        g.resend();
        assert_eq!(g.outbox.len(), 1);
    }

    #[test]
    fn png_size_comes_from_the_header() {
        // The first 24 bytes of a 300x120 PNG.
        let mut head = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        head.extend_from_slice(&300u32.to_be_bytes());
        head.extend_from_slice(&120u32.to_be_bytes());
        let payload = crate::clipboard::base64_for_tests(&head);
        let cmd = Command {
            keys: [("f".to_owned(), "100".to_owned())].into(),
            payload: payload.into_bytes(),
        };
        assert_eq!(pixel_size(&cmd), Some((300, 120)));
        assert_eq!(cell_size(&cmd, pixel_size(&cmd), (10, 20)), Some((30, 6)));
        let given = Command::parse(b"f=100,c=5,r=2;AAAA");
        assert_eq!(cell_size(&given, None, (10, 20)), Some((5, 2)));
    }
}
