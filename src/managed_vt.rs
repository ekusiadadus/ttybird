use std::{cell::RefCell, rc::Rc};

use anyhow::{Context, bail, ensure};
use libghostty_vt::{
    Terminal, TerminalOptions,
    fmt::{Format, Formatter, FormatterOptions},
    key::{Action, Encoder as KeyEncoder, Event as KeyEvent, Key, Mods},
    paste,
    selection::Selection,
    terminal::{Mode, Point, PointCoordinate},
};
use serde::{Deserialize, Serialize};

const MAX_COLS: u16 = 200;
const MAX_ROWS: u16 = 80;
const MAX_SCROLLBACK: usize = 2_000;
const MAX_FEED_BYTES: usize = 1024 * 1024;
const MAX_REPLY_BYTES: usize = 64 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
const MAX_PASTE_BYTES: usize = 1024 * 1024;

/// A serializable, bounded view of a managed terminal's visible screen.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    /// VT that reconstructs only the visible screen, without title, clipboard,
    /// working-directory, or hyperlink OSC sequences.
    pub vt: String,
    /// Zero-based cursor coordinates, omitted when the cursor is hidden.
    pub cursor: Option<(u16, u16)>,
}

/// A normalized key event received from a managed-session client.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct KeyInput {
    pub code: String,
    pub text: Option<String>,
    pub modifiers: u8,
    pub release: bool,
    pub repeat: bool,
}

/// Persistent terminal state for one explicitly managed PTY.
///
/// libghostty-vt is deliberately kept inside this type: callers exchange only
/// owned bytes and serializable screen data, and cannot retain terminal views.
#[derive(Debug)]
pub struct Engine {
    terminal: Terminal<'static, 'static>,
    key_encoder: KeyEncoder<'static>,
    replies: Rc<RefCell<Vec<u8>>>,
    cols: u16,
    rows: u16,
}

impl Engine {
    pub fn new(cols: u16, rows: u16) -> anyhow::Result<Self> {
        validate_dimensions(cols, rows)?;

        let replies = Rc::new(RefCell::new(Vec::new()));
        let callback_replies = Rc::clone(&replies);
        let mut terminal = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: MAX_SCROLLBACK,
        })
        .context("create managed terminal")?;
        terminal
            .set_apc_max_bytes(Some(4096))
            .context("bound managed terminal APC buffer")?
            .set_glyph_protocol_enabled(false)
            .context("disable managed terminal glyph protocol")?
            .on_pty_write(move |_terminal, data| {
                let mut buffered = callback_replies.borrow_mut();
                if data.len() <= MAX_REPLY_BYTES.saturating_sub(buffered.len()) {
                    buffered.extend_from_slice(data);
                }
            })
            .context("install managed terminal PTY reply handler")?;

        Ok(Self {
            terminal,
            key_encoder: KeyEncoder::new().context("create managed terminal key encoder")?,
            replies,
            cols,
            rows,
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        ensure!(
            bytes.len() <= MAX_FEED_BYTES,
            "managed terminal input exceeds {MAX_FEED_BYTES} bytes"
        );
        self.terminal.vt_write(bytes);
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        validate_dimensions(cols, rows)?;
        self.terminal
            .resize(cols, rows, 0, 0)
            .context("resize managed terminal")?;
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    /// Drain bytes that the terminal generated for its PTY, such as DSR replies.
    pub fn replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.replies.borrow_mut())
    }

    pub fn snapshot(&mut self) -> anyhow::Result<Screen> {
        let top_left = self
            .terminal
            .grid_ref(Point::Active(PointCoordinate { x: 0, y: 0 }))
            .context("locate managed terminal viewport start")?;
        let bottom_right = self
            .terminal
            .grid_ref(Point::Active(PointCoordinate {
                x: self.cols - 1,
                y: u32::from(self.rows - 1),
            }))
            .context("locate managed terminal viewport end")?;
        let viewport = Selection::new(top_left, bottom_right, true);
        let options = FormatterOptions::new()
            .with_format(Format::Vt)
            .with_unwrap(false)
            .with_trim(false)
            .with_selection(&viewport)
            .with_cursor(false)
            .with_style(false)
            .with_hyperlink(false)
            .with_modes(false)
            .with_scrolling_region(false)
            .with_tabstops(false)
            .with_pwd(false)
            .with_keyboard(false)
            .with_kitty_keyboard(false)
            .with_charsets(false)
            .with_palette(false)
            .with_protection(false);
        let mut formatter = Formatter::new(&self.terminal, options)
            .context("create managed terminal screen formatter")?;
        // libghostty-vt's format_len wrapper reports InvalidValue when the C
        // formatter succeeds with zero bytes. That is the valid result for a
        // pristine terminal before its child has produced any output.
        let len = match formatter.format_len() {
            Ok(len) => len,
            Err(libghostty_vt::Error::InvalidValue) => 0,
            Err(error) => {
                return Err(error).context("measure managed terminal screen snapshot");
            }
        };
        ensure!(
            len <= MAX_SNAPSHOT_BYTES,
            "managed terminal screen snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes"
        );
        let mut bytes = vec![0; len];
        let written = formatter
            .format_buf(&mut bytes)
            .context("format managed terminal screen snapshot")?;
        bytes.truncate(written);

        let body = String::from_utf8(bytes).context("managed terminal snapshot is not UTF-8")?;
        // Establish a known origin and style for consumers, and leave their
        // parser in the default style even when the final visible cell is styled.
        let vt = format!("\x1b[0m\x1b[2J\x1b[H{body}\x1b[0m");
        ensure!(
            vt.len() <= MAX_SNAPSHOT_BYTES,
            "managed terminal screen snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes"
        );

        let cursor = if self
            .terminal
            .is_cursor_visible()
            .context("read managed terminal cursor visibility")?
        {
            Some((
                self.terminal
                    .cursor_x()
                    .context("read managed terminal cursor column")?,
                self.terminal
                    .cursor_y()
                    .context("read managed terminal cursor row")?,
            ))
        } else {
            None
        };

        Ok(Screen {
            cols: self.cols,
            rows: self.rows,
            vt,
            cursor,
        })
    }

    pub fn encode_key(&mut self, input: &KeyInput) -> anyhow::Result<Vec<u8>> {
        ensure!(
            input.modifiers & !0x0f == 0,
            "unknown key modifier bits: {:#x}",
            input.modifiers & !0x0f
        );
        ensure!(
            !(input.release && input.repeat),
            "key event cannot be both release and repeat"
        );

        let (key, force_shift) = parse_key(input)?;
        let action = if input.release {
            Action::Release
        } else if input.repeat {
            Action::Repeat
        } else {
            Action::Press
        };
        let mut mods = parse_modifiers(input.modifiers);
        if force_shift {
            mods |= Mods::SHIFT;
        }

        let mut event = KeyEvent::new().context("create managed terminal key event")?;
        event
            .set_action(action)
            .set_key(key)
            .set_mods(mods)
            .set_consumed_mods(Mods::empty());
        if input.code == "char" {
            let text = input
                .text
                .as_deref()
                .expect("parse_key validates char text");
            event.set_utf8(Some(text));
            if let Some(codepoint) = text.chars().next() {
                event.set_unshifted_codepoint(unshifted_codepoint(codepoint));
            }
        } else {
            event.set_utf8::<&str>(None);
        }

        self.key_encoder.set_options_from_terminal(&self.terminal);
        let mut encoded = Vec::with_capacity(16);
        self.key_encoder
            .encode_to_vec(&event, &mut encoded)
            .context("encode managed terminal key event")?;
        Ok(encoded)
    }

    pub fn encode_paste(&mut self, text: &str) -> anyhow::Result<Vec<u8>> {
        ensure!(
            text.len() <= MAX_PASTE_BYTES,
            "managed terminal paste exceeds {MAX_PASTE_BYTES} bytes"
        );
        let bracketed = self
            .terminal
            .mode(Mode::BRACKETED_PASTE)
            .context("read managed terminal bracketed paste mode")?;
        let mut input = text.as_bytes().to_vec();
        let capacity = input
            .len()
            .checked_add(12)
            .context("managed terminal paste is too large")?;
        let mut output = vec![0; capacity];
        let written = paste::encode(&mut input, bracketed, &mut output)
            .context("encode managed terminal paste")?;
        output.truncate(written);
        Ok(output)
    }
}

fn validate_dimensions(cols: u16, rows: u16) -> anyhow::Result<()> {
    ensure!(
        cols > 0 && rows > 0,
        "managed terminal dimensions must be non-zero"
    );
    ensure!(
        cols <= MAX_COLS,
        "managed terminal width exceeds {MAX_COLS} columns"
    );
    ensure!(
        rows <= MAX_ROWS,
        "managed terminal height exceeds {MAX_ROWS} rows"
    );
    Ok(())
}

fn parse_modifiers(value: u8) -> Mods {
    let mut mods = Mods::empty();
    if value & 1 != 0 {
        mods |= Mods::SHIFT;
    }
    if value & 2 != 0 {
        mods |= Mods::ALT;
    }
    if value & 4 != 0 {
        mods |= Mods::CTRL;
    }
    if value & 8 != 0 {
        mods |= Mods::SUPER;
    }
    mods
}

fn parse_key(input: &KeyInput) -> anyhow::Result<(Key, bool)> {
    let key = match input.code.as_str() {
        "char" => {
            let text = input.text.as_deref().context("char key requires text")?;
            ensure!(
                text.chars().count() == 1,
                "char key text must contain exactly one Unicode scalar value"
            );
            let value = text.chars().next().expect("validated one character");
            ensure!(
                !value.is_control(),
                "char key text cannot be a control character"
            );
            key_for_char(value)
        }
        "enter" => Key::Enter,
        "backspace" => Key::Backspace,
        "tab" => Key::Tab,
        "backtab" => return Ok((Key::Tab, true)),
        "esc" => Key::Escape,
        "up" => Key::ArrowUp,
        "down" => Key::ArrowDown,
        "left" => Key::ArrowLeft,
        "right" => Key::ArrowRight,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "insert" => Key::Insert,
        "delete" => Key::Delete,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        code => bail!("unsupported key code: {code}"),
    };
    Ok((key, false))
}

fn key_for_char(value: char) -> Key {
    match value.to_ascii_lowercase() {
        'a' => Key::A,
        'b' => Key::B,
        'c' => Key::C,
        'd' => Key::D,
        'e' => Key::E,
        'f' => Key::F,
        'g' => Key::G,
        'h' => Key::H,
        'i' => Key::I,
        'j' => Key::J,
        'k' => Key::K,
        'l' => Key::L,
        'm' => Key::M,
        'n' => Key::N,
        'o' => Key::O,
        'p' => Key::P,
        'q' => Key::Q,
        'r' => Key::R,
        's' => Key::S,
        't' => Key::T,
        'u' => Key::U,
        'v' => Key::V,
        'w' => Key::W,
        'x' => Key::X,
        'y' => Key::Y,
        'z' => Key::Z,
        '0' => Key::Digit0,
        '1' => Key::Digit1,
        '2' => Key::Digit2,
        '3' => Key::Digit3,
        '4' => Key::Digit4,
        '5' => Key::Digit5,
        '6' => Key::Digit6,
        '7' => Key::Digit7,
        '8' => Key::Digit8,
        '9' => Key::Digit9,
        '`' | '~' => Key::Backquote,
        '\\' | '|' => Key::Backslash,
        '[' | '{' => Key::BracketLeft,
        ']' | '}' => Key::BracketRight,
        ',' | '<' => Key::Comma,
        '=' | '+' => Key::Equal,
        '-' | '_' => Key::Minus,
        '.' | '>' => Key::Period,
        '\'' | '"' => Key::Quote,
        ';' | ':' => Key::Semicolon,
        '/' | '?' => Key::Slash,
        ' ' => Key::Space,
        _ => Key::Unidentified,
    }
}

fn unshifted_codepoint(value: char) -> char {
    match value {
        'A'..='Z' => value.to_ascii_lowercase(),
        '~' => '`',
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        _ => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: &str) -> KeyInput {
        KeyInput {
            code: code.to_owned(),
            text: None,
            modifiers: 0,
            release: false,
            repeat: false,
        }
    }

    #[test]
    fn snapshots_blank_terminal_before_any_output() {
        let mut engine = Engine::new(80, 24).unwrap();
        let screen = engine.snapshot().unwrap();

        assert_eq!((screen.cols, screen.rows), (80, 24));
        assert!(screen.vt.starts_with("\x1b[0m\x1b[2J\x1b[H"));
        assert_eq!(screen.cursor, Some((0, 0)));
    }

    #[test]
    fn preserves_split_sequences_unicode_color_and_alternate_screen() {
        let mut engine = Engine::new(12, 3).unwrap();
        engine.feed(b"main ").unwrap();
        engine.feed(b"\x1b[31").unwrap();
        engine.feed("m赤\x1b[0m".as_bytes()).unwrap();
        let primary = engine.snapshot().unwrap();
        assert!(primary.vt.contains("main"));
        assert!(primary.vt.contains("赤"));
        assert!(
            primary.vt.contains("\x1b[31m") || primary.vt.contains("\x1b[38;"),
            "snapshot did not preserve red foreground: {:?}",
            primary.vt
        );

        engine.feed(b"\x1b[?1049hALT").unwrap();
        let alternate = engine.snapshot().unwrap();
        assert!(alternate.vt.contains("ALT"));
        assert!(!alternate.vt.contains("main"));

        engine.feed(b"\x1b[?1049l").unwrap();
        assert!(engine.snapshot().unwrap().vt.contains("main"));
    }

    #[test]
    fn resize_reflows_and_enforces_bounds() {
        let mut engine = Engine::new(8, 3).unwrap();
        engine.feed(b"abcdefgh1234").unwrap();
        engine.resize(4, 4).unwrap();
        let screen = engine.snapshot().unwrap();
        assert_eq!((screen.cols, screen.rows), (4, 4));
        assert!(screen.vt.contains("abcd"));
        assert!(Engine::new(MAX_COLS + 1, 1).is_err());
        assert!(engine.resize(1, MAX_ROWS + 1).is_err());
    }

    #[test]
    fn snapshot_contains_only_the_active_viewport_not_scrollback() {
        let mut engine = Engine::new(8, 2).unwrap();
        engine.feed(b"one\r\ntwo\r\nthree").unwrap();
        let screen = engine.snapshot().unwrap();
        assert!(!screen.vt.contains("one"));
        assert!(screen.vt.contains("two"));
        assert!(screen.vt.contains("three"));
    }

    #[test]
    fn drains_terminal_query_replies() {
        let mut engine = Engine::new(10, 3).unwrap();
        engine.feed(b"abc\x1b[6n").unwrap();
        assert_eq!(engine.replies(), b"\x1b[1;4R");
        assert!(engine.replies().is_empty());
    }

    #[test]
    fn key_encoding_tracks_application_cursor_and_kitty_state() {
        let mut engine = Engine::new(10, 3).unwrap();
        assert_eq!(engine.encode_key(&key("up")).unwrap(), b"\x1b[A");
        engine.feed(b"\x1b[?1h").unwrap();
        assert_eq!(engine.encode_key(&key("up")).unwrap(), b"\x1bOA");

        // disambiguate + report events + report all, so a release is emitted.
        engine.feed(b"\x1b[>11u").unwrap();
        let mut enter = key("enter");
        enter.release = true;
        assert_eq!(engine.encode_key(&enter).unwrap(), b"\x1b[13;1:3u");
    }

    #[test]
    fn encodes_utf8_keys_and_terminal_paste_mode() {
        let mut engine = Engine::new(10, 3).unwrap();
        let mut input = key("char");
        input.text = Some("界".to_owned());
        assert_eq!(engine.encode_key(&input).unwrap(), "界".as_bytes());

        assert_eq!(engine.encode_paste("a\nb").unwrap(), b"a\rb");
        engine.feed(b"\x1b[?2004h").unwrap();
        assert_eq!(
            engine.encode_paste("a\nb").unwrap(),
            b"\x1b[200~a\nb\x1b[201~"
        );
    }

    #[test]
    fn snapshot_omits_title_clipboard_and_hidden_cursor() {
        let mut engine = Engine::new(10, 2).unwrap();
        engine
            .feed(b"ok\x1b]2;secret title\x1b\\\x1b]52;c;c2VjcmV0\x07\x1b[?25l")
            .unwrap();
        let screen = engine.snapshot().unwrap();
        assert!(screen.vt.contains("ok"));
        assert!(!screen.vt.contains("secret"));
        assert!(!screen.vt.contains("]2;"));
        assert!(!screen.vt.contains("]52;"));
        assert_eq!(screen.cursor, None);
    }
}
