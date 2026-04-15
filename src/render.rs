use ansi_term::{Colour, Style};
use unicode_width::UnicodeWidthStr;
use zellij_tile::prelude::*;

use crate::status::AgentStatusKind;

#[derive(Clone, Debug)]
pub struct TabLine {
    pub position: usize,
    pub name: String,
    pub active: bool,
    pub unread: bool,
    pub tracked_agents: usize,
    pub status: AgentStatusKind,
}

pub fn render_sidebar(
    rows: usize,
    cols: usize,
    mode_info: &ModeInfo,
    tabs: &[TabLine],
) -> Vec<Option<usize>> {
    let palette = mode_info.style.colors;
    let base_bg = palette.text_unselected.background;
    let base_fg = palette.text_unselected.base;
    let active_bg = palette.ribbon_selected.background;
    let active_fg = palette.ribbon_selected.base;
    let accent = palette.ribbon_unselected.emphasis_3;

    let mut clickable_rows = Vec::new();
    let header = format!(
        " {} ",
        mode_info.session_name.as_deref().unwrap_or("zeldex")
    );
    println!(
        "{}",
        paint(truncate_to_width(&header, cols), base_fg, base_bg, true,)
    );
    clickable_rows.push(None);

    for tab in tabs.iter().take(rows.saturating_sub(1)) {
        let status = if tab.unread {
            AgentStatusKind::Waiting
        } else {
            tab.status
        };
        let badge = if tab.unread { "●" } else { status.badge() };
        let label = if cols >= 18 { status.label() } else { "" };
        let count = if tab.tracked_agents > 1 {
            format!(" {}", tab.tracked_agents)
        } else {
            String::new()
        };
        let prefix = if tab.active { "▌" } else { " " };
        let suffix = if label.is_empty() {
            format!("{badge}{count}")
        } else {
            format!("{badge}{count} {label}")
        };
        let index = tab.position + 1;
        let chrome_width = UnicodeWidthStr::width(prefix)
            + 1
            + index.to_string().len()
            + 1
            + UnicodeWidthStr::width(suffix.as_str())
            + 1;
        let available = cols.saturating_sub(chrome_width);
        let name = truncate_to_width(&tab.name, available.max(1));
        let padding = " ".repeat(cols.saturating_sub(
            UnicodeWidthStr::width(prefix)
                + 1
                + index.to_string().len()
                + 1
                + UnicodeWidthStr::width(name.as_str())
                + 1
                + UnicodeWidthStr::width(suffix.as_str()),
        ));
        let line = format!("{prefix} {index} {name}{padding} {suffix}");
        let (fg, bg, bold) = if tab.active {
            (active_fg, active_bg, true)
        } else {
            (base_fg, base_bg, false)
        };
        let rendered = paint(truncate_to_width(&line, cols), fg, bg, bold);
        if tab.unread || matches!(status, AgentStatusKind::Waiting) {
            println!("{}", paint_badge(rendered, accent));
        } else {
            println!("{rendered}");
        }
        clickable_rows.push(Some(tab.position));
    }

    for _ in clickable_rows.len()..rows {
        println!("{}", paint(" ".repeat(cols), base_fg, base_bg, false));
        clickable_rows.push(None);
    }

    clickable_rows
}

pub fn render_notice(
    rows: usize,
    cols: usize,
    mode_info: &ModeInfo,
    lines: &[String],
) -> Vec<Option<usize>> {
    let mut clickable_rows = Vec::new();
    let header = fill_to_width(
        &format!(
            " {} ",
            mode_info.session_name.as_deref().unwrap_or("zeldex")
        ),
        cols,
    );
    println!("{header}");
    clickable_rows.push(None);

    for line in lines.iter().take(rows.saturating_sub(1)) {
        println!("{}", fill_to_width(&format!(" {line}"), cols));
        clickable_rows.push(None);
    }

    for _ in clickable_rows.len()..rows {
        println!("{}", " ".repeat(cols));
        clickable_rows.push(None);
    }

    clickable_rows
}

fn paint(text: String, fg: PaletteColor, bg: PaletteColor, bold: bool) -> String {
    let mut style = Style::new().fg(to_colour(fg)).on(to_colour(bg));
    if bold {
        style = style.bold();
    }
    style.paint(text).to_string()
}

fn paint_badge(text: String, accent: PaletteColor) -> String {
    format!("{}{}", Style::new().fg(to_colour(accent)).paint(""), text)
}

fn to_colour(color: PaletteColor) -> Colour {
    match color {
        PaletteColor::Rgb((r, g, b)) => Colour::RGB(r, g, b),
        PaletteColor::EightBit(code) => Colour::Fixed(code),
    }
}

fn truncate_to_width(input: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in input.chars() {
        let next = format!("{out}{ch}");
        if UnicodeWidthStr::width(next.as_str()) > width {
            break;
        }
        out.push(ch);
    }
    out
}

fn fill_to_width(input: &str, width: usize) -> String {
    let truncated = truncate_to_width(input, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(truncated.as_str()));
    format!("{truncated}{}", " ".repeat(padding))
}
