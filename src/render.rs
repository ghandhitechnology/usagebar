//! Headless frame dump. Renders one frame into an offscreen buffer and writes it back out
//! as ANSI, so the UI can be inspected without a terminal or a tmux server in the way.

use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use crate::ui::{self, App};

pub fn to_ansi(width: u16, height: u16, app: &App) -> Result<String, std::convert::Infallible> {
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| ui::draw(frame, app))?;

    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        let mut current: Option<(Option<String>, Option<String>, bool)> = None;
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            let style = (
                colour(cell.fg),
                colour(cell.bg),
                cell.modifier.contains(Modifier::BOLD),
            );
            if current.as_ref() != Some(&style) {
                line.push_str(&sgr(&style));
                current = Some(style);
            }
            line.push_str(cell.symbol());
        }
        out.push_str(line.trim_end());
        out.push_str("\x1b[0m\n");
    }
    Ok(out)
}

fn sgr(style: &(Option<String>, Option<String>, bool)) -> String {
    let mut codes = vec!["0".to_string()];
    if style.2 {
        codes.push("1".into());
    }
    if let Some(fg) = &style.0 {
        codes.push(fg.clone());
    }
    if let Some(bg) = &style.1 {
        codes.push(bg.clone());
    }
    format!("\x1b[{}m", codes.join(";"))
}

fn colour(value: Color) -> Option<String> {
    match value {
        Color::Rgb(r, g, b) => Some(format!("38;2;{r};{g};{b}")),
        Color::Reset => None,
        other => Some(format!("38;5;{}", indexed(other))),
    }
}

fn indexed(value: Color) -> u8 {
    match value {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::White => 7,
        Color::Gray => 8,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        _ => 15,
    }
}
