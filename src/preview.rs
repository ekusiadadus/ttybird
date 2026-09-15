use anyhow::{Context, ensure};
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::{StyleColor, Underline};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

const MAX_COLS: u16 = 500;
const MAX_ROWS: u16 = 200;
const MAX_CELLS: usize = 500 * 200;
const MAX_INPUT_BYTES: usize = 1024 * 1024;

/// Parse a tmux visible-screen capture into styled, owned Ratatui text.
///
/// tmux separates captured rows with LF, while a terminal LF moves down without
/// returning to column zero. Bare LF is therefore converted to CRLF before it
/// is fed to the terminal emulator. A final line ending is a capture delimiter,
/// not a request to scroll the emulated screen, so it is omitted.
pub fn parse(bytes: &[u8], cols: u16, rows: u16) -> anyhow::Result<Text<'static>> {
    validate_limits(bytes, cols, rows)?;
    let input = normalize_tmux_line_endings(bytes);

    // libghostty-vt handles are !Send and !Sync. Keep their entire lifetime in
    // this call and return only owned Ratatui strings and styles.
    let mut terminal = Terminal::new(TerminalOptions {
        cols,
        rows,
        max_scrollback: 0,
    })
    .context("create preview terminal")?;
    terminal.vt_write(&input);

    let mut render_state = RenderState::new().context("create preview render state")?;
    let mut row_iterator = RowIterator::new().context("create preview row iterator")?;
    let mut cell_iterator = CellIterator::new().context("create preview cell iterator")?;
    let snapshot = render_state
        .update(&terminal)
        .context("render parsed preview")?;
    let palette = snapshot
        .colors()
        .context("read preview color palette")?
        .palette;
    let mut rendered_rows = row_iterator
        .update(&snapshot)
        .context("read preview rows")?;
    let mut lines = Vec::with_capacity(rows as usize);

    while let Some(row) = rendered_rows.next() {
        let mut rendered_cells = cell_iterator
            .update(row)
            .context("read preview row cells")?;
        let mut spans: Vec<Span<'static>> = Vec::new();

        while let Some(cell) = rendered_cells.next() {
            let raw_cell = cell.raw_cell().context("read preview cell")?;
            if raw_cell.wide().context("read preview cell width")? == CellWide::SpacerTail {
                continue;
            }

            let mut content = String::new();
            cell.graphemes_utf8(&mut content)
                .context("read preview cell text")?;
            if content.is_empty() {
                content.push(' ');
            }

            let ghostty_style = cell.style().context("read preview cell style")?;
            let style = convert_style(
                ghostty_style,
                cell.fg_color().context("read preview foreground")?,
                cell.bg_color().context("read preview background")?,
                &palette,
            );
            push_merged_span(&mut spans, content, style);
        }

        trim_unstyled_trailing_spaces(&mut spans);
        lines.push(Line::from(spans));
    }

    // The iterator currently yields the complete viewport. Preserve that
    // contract defensively if a future library version omits empty tail rows.
    lines.resize_with(rows as usize, Line::default);
    lines.truncate(rows as usize);
    Ok(Text::from(lines))
}

fn validate_limits(bytes: &[u8], cols: u16, rows: u16) -> anyhow::Result<()> {
    ensure!(cols > 0 && rows > 0, "preview dimensions must be non-zero");
    ensure!(cols <= MAX_COLS, "preview width exceeds {MAX_COLS} columns");
    ensure!(rows <= MAX_ROWS, "preview height exceeds {MAX_ROWS} rows");
    ensure!(
        usize::from(cols) * usize::from(rows) <= MAX_CELLS,
        "preview dimensions exceed {MAX_CELLS} cells"
    );
    ensure!(
        bytes.len() <= MAX_INPUT_BYTES,
        "preview input exceeds {MAX_INPUT_BYTES} bytes"
    );
    Ok(())
}

fn normalize_tmux_line_endings(bytes: &[u8]) -> Vec<u8> {
    let content = bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(bytes);
    let mut normalized = Vec::with_capacity(content.len().saturating_add(content.len() / 8));

    for (index, byte) in content.iter().copied().enumerate() {
        if byte == b'\n' && (index == 0 || content[index - 1] != b'\r') {
            normalized.push(b'\r');
        }
        normalized.push(byte);
    }
    normalized
}

fn convert_style(
    ghostty: libghostty_vt::style::Style,
    foreground: Option<libghostty_vt::style::RgbColor>,
    background: Option<libghostty_vt::style::RgbColor>,
    palette: &[libghostty_vt::style::RgbColor; 256],
) -> Style {
    let mut style = Style::default();
    if let Some(color) = foreground {
        style = style.fg(rgb(color));
    }
    if let Some(color) = background {
        style = style.bg(rgb(color));
    }
    match ghostty.underline_color {
        StyleColor::None => {}
        StyleColor::Palette(index) => {
            style = style.underline_color(rgb(palette[index.0 as usize]));
        }
        StyleColor::Rgb(color) => {
            style = style.underline_color(rgb(color));
        }
    }

    let mut modifiers = Modifier::empty();
    if ghostty.bold {
        modifiers |= Modifier::BOLD;
    }
    if ghostty.faint {
        modifiers |= Modifier::DIM;
    }
    if ghostty.italic {
        modifiers |= Modifier::ITALIC;
    }
    if ghostty.underline != Underline::None {
        modifiers |= Modifier::UNDERLINED;
    }
    if ghostty.blink {
        modifiers |= Modifier::SLOW_BLINK;
    }
    if ghostty.inverse {
        modifiers |= Modifier::REVERSED;
    }
    if ghostty.invisible {
        modifiers |= Modifier::HIDDEN;
    }
    if ghostty.strikethrough {
        modifiers |= Modifier::CROSSED_OUT;
    }
    style.add_modifier(modifiers)
}

fn rgb(color: libghostty_vt::style::RgbColor) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

fn push_merged_span(spans: &mut Vec<Span<'static>>, content: String, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(&content);
        return;
    }
    spans.push(Span::styled(content, style));
}

fn trim_unstyled_trailing_spaces(spans: &mut Vec<Span<'static>>) {
    while let Some(last) = spans.last_mut() {
        if last.style != Style::default() {
            break;
        }
        let trimmed_len = last.content.trim_end_matches(' ').len();
        last.content.to_mut().truncate(trimmed_len);
        if last.content.is_empty() {
            spans.pop();
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn preserves_rows_from_tmux_lf_capture_without_scrolling() {
        let text = parse(b"first\nsecond\nthird\n", 20, 3).unwrap();

        assert_eq!(text.lines.len(), 3);
        assert_eq!(content(&text.lines[0]), "first");
        assert_eq!(content(&text.lines[1]), "second");
        assert_eq!(content(&text.lines[2]), "third");
    }

    #[test]
    fn lf_after_full_width_row_does_not_trigger_pending_wrap() {
        let text = parse(b"12345\nnext\n", 5, 2).unwrap();

        assert_eq!(content(&text.lines[0]), "12345");
        assert_eq!(content(&text.lines[1]), "next");
    }

    #[test]
    fn renders_unicode_wide_and_combining_graphemes_once() {
        let text = parse("A界 e\u{301} 🙂\n".as_bytes(), 20, 2).unwrap();

        assert_eq!(content(&text.lines[0]), "A界 e\u{301} 🙂");
    }

    #[test]
    fn converts_sgr_colors_and_modifiers() {
        let text = parse(
            b"plain \x1b[1;3;38;2;12;34;56;48;2;70;80;90mstyled\x1b[0m\n",
            30,
            2,
        )
        .unwrap();
        let styled = text.lines[0]
            .spans
            .iter()
            .find(|span| span.content.contains("styled"))
            .unwrap();

        assert_eq!(styled.style.fg, Some(Color::Rgb(12, 34, 56)));
        assert_eq!(styled.style.bg, Some(Color::Rgb(70, 80, 90)));
        assert!(styled.style.add_modifier.contains(Modifier::BOLD));
        assert!(styled.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn applies_cursor_motion_and_erasure() {
        let text = parse(
            b"abcdef\rXY\x1b[2C!\x1b[K\r\nsecond\x1b[1A\r\x1b[2KUP\n",
            12,
            3,
        )
        .unwrap();

        assert_eq!(content(&text.lines[0]), "UP");
        assert_eq!(content(&text.lines[1]), "second");
        assert_eq!(content(&text.lines[2]), "");

        let text = parse(b"old\r\nsecret\x1b[2J\x1b[Hnew\n", 12, 2).unwrap();
        assert_eq!(content(&text.lines[0]), "new");
        assert_eq!(content(&text.lines[1]), "");
    }

    #[test]
    fn switches_and_restores_the_alternate_screen() {
        let active = parse(b"primary\x1b[?1049h\x1b[2J\x1b[Halternate", 16, 2).unwrap();
        assert_eq!(content(&active.lines[0]), "alternate");

        let restored = parse(
            b"primary\x1b[?1049h\x1b[2J\x1b[Halternate\x1b[?1049l",
            16,
            2,
        )
        .unwrap();
        assert_eq!(content(&restored.lines[0]), "primary");
        assert!(!restored.to_string().contains("alternate"));
    }

    #[test]
    fn overwriting_a_wide_character_tail_clears_the_grapheme() {
        let text = parse("A界Z\r\x1b[2CX\n".as_bytes(), 6, 2).unwrap();

        assert_eq!(content(&text.lines[0]), "A XZ");
        assert!(!content(&text.lines[0]).contains('界'));
    }

    #[test]
    fn terminal_reset_clears_cells_and_style() {
        let text = parse(b"\x1b[31mold\x1bcnew\n", 10, 2).unwrap();

        assert_eq!(content(&text.lines[0]), "new");
        assert_eq!(text.lines[0].spans[0].style, Style::default());
    }

    #[test]
    fn drops_side_effect_control_strings_queries_and_unknown_modes() {
        let text = parse(
            b"ok\x1b]2;title\x07\x1bP1;2|payload\x1b\\\x1b_hidden\x1b\\\x1b[5n\x1b[?9999h done\n",
            20,
            2,
        )
        .unwrap();

        assert_eq!(content(&text.lines[0]), "ok done");
        assert!(!text.to_string().contains('\x1b'));
        assert!(!text.to_string().contains("payload"));
    }

    #[test]
    fn rejects_invalid_dimensions_and_oversized_input() {
        assert!(parse(b"", 0, 1).is_err());
        assert!(parse(b"", MAX_COLS + 1, 1).is_err());
        assert!(parse(b"", 1, MAX_ROWS + 1).is_err());
        assert!(parse(&vec![b'x'; MAX_INPUT_BYTES + 1], 80, 24).is_err());
    }
}
