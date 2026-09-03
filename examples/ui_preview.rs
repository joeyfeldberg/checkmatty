//! Render the cell-based UI to an SVG for deterministic visual review.

use std::{env, error::Error, fmt::Write as _, fs, path::PathBuf};

use chess_tui::{
    app::{App, Command},
    ui::{self, UiState},
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    style::{Color, Modifier},
};

const CELL_WIDTH: u16 = 10;
const CELL_HEIGHT: u16 = 20;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let output = arguments
        .next()
        .map_or_else(|| PathBuf::from("chess-ui-preview.svg"), PathBuf::from);
    let columns = arguments
        .next()
        .and_then(|value| value.to_string_lossy().parse().ok())
        .unwrap_or(80);
    let rows = arguments
        .next()
        .and_then(|value| value.to_string_lossy().parse().ok())
        .unwrap_or(24);

    let mut app = App::new();
    app.dispatch(Command::Activate);
    let mut state = UiState::unicode();
    let mut terminal = Terminal::new(TestBackend::new(columns, rows))?;
    terminal.draw(|frame| ui::render(frame, &app, &mut state, "unicode"))?;
    let buffer = terminal.backend().buffer();

    let width = u32::from(columns) * u32::from(CELL_WIDTH);
    let height = u32::from(rows) * u32::from(CELL_HEIGHT);
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\">\n"
    );
    svg.push_str("<rect width=\"100%\" height=\"100%\" fill=\"#0a0f1b\"/>\n");

    for y in 0..rows {
        for x in 0..columns {
            let cell = &buffer[(x, y)];
            let left = u32::from(x) * u32::from(CELL_WIDTH);
            let top = u32::from(y) * u32::from(CELL_HEIGHT);
            if let Some(background) = svg_color(cell.bg) {
                writeln!(
                    svg,
                    "<rect x=\"{left}\" y=\"{top}\" width=\"{CELL_WIDTH}\" height=\"{CELL_HEIGHT}\" fill=\"{background}\"/>"
                )?;
            }

            let symbol = cell.symbol();
            if symbol != " " && !symbol.is_empty() {
                let foreground = svg_color(cell.fg).unwrap_or_else(|| "#e2e8f0".to_owned());
                let baseline = top + 16;
                let weight = if cell.modifier.contains(Modifier::BOLD) {
                    "700"
                } else {
                    "400"
                };
                writeln!(
                    svg,
                    "<text x=\"{left}\" y=\"{baseline}\" fill=\"{foreground}\" font-family=\"Menlo, Apple Symbols, monospace\" font-size=\"16\" font-weight=\"{weight}\">{}</text>",
                    escape_xml(symbol)
                )?;
            }
        }
    }
    svg.push_str("</svg>\n");
    fs::write(&output, svg)?;
    println!("wrote {}", output.display());
    Ok(())
}

fn svg_color(color: Color) -> Option<String> {
    match color {
        Color::Reset => None,
        Color::Black => Some("#000000".to_owned()),
        Color::Red => Some("#ef4444".to_owned()),
        Color::Green => Some("#22c55e".to_owned()),
        Color::Yellow => Some("#eab308".to_owned()),
        Color::Blue => Some("#3b82f6".to_owned()),
        Color::Magenta => Some("#d946ef".to_owned()),
        Color::Cyan => Some("#06b6d4".to_owned()),
        Color::Gray => Some("#94a3b8".to_owned()),
        Color::DarkGray => Some("#475569".to_owned()),
        Color::LightRed => Some("#f87171".to_owned()),
        Color::LightGreen => Some("#4ade80".to_owned()),
        Color::LightYellow => Some("#facc15".to_owned()),
        Color::LightBlue => Some("#60a5fa".to_owned()),
        Color::LightMagenta => Some("#d946ef".to_owned()),
        Color::LightCyan => Some("#22d3ee".to_owned()),
        Color::White => Some("#ffffff".to_owned()),
        Color::Rgb(red, green, blue) => Some(format!("#{red:02x}{green:02x}{blue:02x}")),
        Color::Indexed(_) => Some("#94a3b8".to_owned()),
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
