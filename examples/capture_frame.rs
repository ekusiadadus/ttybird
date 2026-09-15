//! Render a captured, allowlisted tmux screen through the real VT parser.
use ratatui::{Terminal, backend::TestBackend, style::Color, widgets::Paragraph};
use std::io::{self, Read};
fn rgb(color: Color, background: bool) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Black => "#101820".into(),
        Color::White | Color::Gray => "#dce7ed".into(),
        _ => {
            if background {
                "#101820".into()
            } else {
                "#dce7ed".into()
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cols: u16 = std::env::args().nth(1).unwrap_or("160".into()).parse()?;
    let rows: u16 = std::env::args().nth(2).unwrap_or("36".into()).parse()?;
    let mut bytes = Vec::new();
    io::stdin().take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    let text = ttybird::preview::parse(&bytes, cols, rows)?;
    let mut terminal = Terminal::new(TestBackend::new(cols, rows))?;
    terminal.draw(|frame| frame.render_widget(Paragraph::new(text.clone()), frame.area()))?;
    let cells: Vec<_> = terminal.backend().buffer().content.iter().enumerate().map(|(i,c)|
        serde_json::json!({"x":i%usize::from(cols),"y":i/usize::from(cols),"text":c.symbol(),"fg":rgb(c.fg,false),"bg":rgb(c.bg,true)})
    ).collect();
    println!(
        "{}",
        serde_json::json!({"width":cols,"height":rows,"cells":cells})
    );
    Ok(())
}
