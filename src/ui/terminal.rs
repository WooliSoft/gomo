use ratatui::{
    buffer::{Buffer, Cell as BufferCell},
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Cell as TableCell, Widget},
};

pub(crate) fn render_widget_to_string(widget: impl Widget, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    widget.render(area, &mut buffer);

    let mut output = String::new();
    for y in 0..height {
        let line_end = (0..width)
            .rfind(|&x| buffer[(x, y)].symbol() != " ")
            .map(|x| x + 1)
            .unwrap_or(0);
        let mut current_style = None;

        for x in 0..line_end {
            let cell = &buffer[(x, y)];
            let next_style = ansi_style(cell);
            if next_style != current_style {
                if current_style.is_some() {
                    output.push_str("\x1b[0m");
                }
                if let Some(style) = next_style {
                    output.push_str(&ansi_start(style));
                }
                current_style = next_style;
            }
            output.push_str(cell.symbol());
        }

        if current_style.is_some() {
            output.push_str("\x1b[0m");
        }
        output.push('\n');
    }
    output
}

pub(crate) fn header_cell(text: &'static str) -> TableCell<'static> {
    TableCell::from(text).style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
}

pub(crate) fn separator_cell() -> TableCell<'static> {
    TableCell::from("│").style(line_style())
}

pub(crate) fn line_style() -> Style {
    Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM)
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct AnsiStyle {
    fg: Color,
    bg: Color,
    modifier: Modifier,
}

fn ansi_style(cell: &BufferCell) -> Option<AnsiStyle> {
    if cell.fg == Color::Reset && cell.bg == Color::Reset && cell.modifier.is_empty() {
        return None;
    }

    Some(AnsiStyle {
        fg: cell.fg,
        bg: cell.bg,
        modifier: cell.modifier,
    })
}

fn ansi_start(style: AnsiStyle) -> String {
    let mut codes = Vec::new();
    if style.modifier.contains(Modifier::BOLD) {
        codes.push("1".to_string());
    }
    if style.modifier.contains(Modifier::DIM) {
        codes.push("2".to_string());
    }
    if style.modifier.contains(Modifier::ITALIC) {
        codes.push("3".to_string());
    }
    if style.modifier.contains(Modifier::UNDERLINED) {
        codes.push("4".to_string());
    }
    if style.modifier.contains(Modifier::REVERSED) {
        codes.push("7".to_string());
    }
    if style.modifier.contains(Modifier::HIDDEN) {
        codes.push("8".to_string());
    }
    if style.modifier.contains(Modifier::CROSSED_OUT) {
        codes.push("9".to_string());
    }
    if let Some(code) = ansi_color(style.fg, true) {
        codes.push(code);
    }
    if let Some(code) = ansi_color(style.bg, false) {
        codes.push(code);
    }

    format!("\x1b[{}m", codes.join(";"))
}

fn ansi_color(color: Color, foreground: bool) -> Option<String> {
    let code = match color {
        Color::Reset => return None,
        Color::Black => 30,
        Color::Red => 31,
        Color::Green => 32,
        Color::Yellow => 33,
        Color::Blue => 34,
        Color::Magenta => 35,
        Color::Cyan => 36,
        Color::Gray => 37,
        Color::DarkGray => 90,
        Color::LightRed => 91,
        Color::LightGreen => 92,
        Color::LightYellow => 93,
        Color::LightBlue => 94,
        Color::LightMagenta => 95,
        Color::LightCyan => 96,
        Color::White => 97,
        Color::Indexed(value) => {
            let prefix = if foreground { 38 } else { 48 };
            return Some(format!("{prefix};5;{value}"));
        }
        Color::Rgb(red, green, blue) => {
            let prefix = if foreground { 38 } else { 48 };
            return Some(format!("{prefix};2;{red};{green};{blue}"));
        }
    };

    if foreground {
        Some(code.to_string())
    } else {
        Some((code + 10).to_string())
    }
}
