//! The subset of lipgloss block styling the views use, rendered the same way:
//! SGR per line, padding, minimum height, left alignment, rounded border.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::ansi;

/// Off by default so tests compare plain text, like lipgloss without a TTY.
static COLOR: AtomicBool = AtomicBool::new(false);

pub fn enable_color(enabled: bool) {
    COLOR.store(enabled, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(u8, u8, u8);

impl Color {
    pub const fn hex(rgb: u32) -> Self {
        Self((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
    }

    fn sgr(self, layer: u8) -> String {
        format!("{layer}8;2;{};{};{}", channel(self.0), channel(self.1), channel(self.2))
    }
}

/// termenv round-trips hex channels through floats and truncates, which
/// turns some values (0x94 → 147) down by one; match its output exactly.
fn channel(value: u8) -> u8 {
    (f64::from(value) * (1.0 / 255.0) * 255.0) as u8
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Style {
    bold: bool,
    fg: Option<Color>,
    bg: Option<Color>,
    width: usize,
    height: usize,
    padding_v: usize,
    padding_h: usize,
    border: bool,
    border_fg: Option<Color>,
}

impl Style {
    pub const fn new() -> Self {
        Self {
            bold: false,
            fg: None,
            bg: None,
            width: 0,
            height: 0,
            padding_v: 0,
            padding_h: 0,
            border: false,
            border_fg: None,
        }
    }

    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub const fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub const fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    /// Block width including padding, excluding the border.
    pub const fn width(mut self, width: usize) -> Self {
        self.width = width;
        self
    }

    /// Minimum block height including padding, excluding the border.
    pub const fn height(mut self, height: usize) -> Self {
        self.height = height;
        self
    }

    pub const fn padding(mut self, vertical: usize, horizontal: usize) -> Self {
        self.padding_v = vertical;
        self.padding_h = horizontal;
        self
    }

    pub const fn border(mut self, color: Color) -> Self {
        self.border = true;
        self.border_fg = Some(color);
        self
    }

    pub fn render(&self, text: &str) -> String {
        let text = text.replace('\t', "    ").replace("\r\n", "\n");
        let colors = COLOR.load(Ordering::Relaxed);
        let text = if self.width > 0 { ansi::wrap(&text, self.width.saturating_sub(2 * self.padding_h)) } else { text };
        let core = if colors { self.text_sgr() } else { String::new() };
        let whitespace = match (colors, self.bg) {
            (true, Some(bg)) => bg.sgr(4),
            _ => String::new(),
        };
        let mut lines: Vec<String> = text.split('\n').map(|line| styled(line, &core)).collect();
        if self.padding_h > 0 {
            let pad = styled(&" ".repeat(self.padding_h), &whitespace);
            for line in &mut lines {
                *line = format!("{pad}{line}{pad}");
            }
        }
        for _ in 0..self.padding_v {
            lines.insert(0, String::new());
            lines.push(String::new());
        }
        if self.height > lines.len() {
            lines.resize(self.height, String::new());
        }
        if lines.len() > 1 || self.width > 0 {
            let widest = lines.iter().map(|l| ansi::width(l)).max().unwrap_or(0);
            let target = widest.max(self.width);
            for line in &mut lines {
                let short = target - ansi::width(line);
                if short > 0 {
                    line.push_str(&styled(&" ".repeat(short), &whitespace));
                }
            }
        }
        if self.border { self.apply_border(&lines, colors) } else { lines.join("\n") }
    }

    fn text_sgr(&self) -> String {
        let mut parts = Vec::new();
        if self.bold {
            parts.push("1".to_string());
        }
        if let Some(fg) = self.fg {
            parts.push(fg.sgr(3));
        }
        if let Some(bg) = self.bg {
            parts.push(bg.sgr(4));
        }
        parts.join(";")
    }

    fn apply_border(&self, lines: &[String], colors: bool) -> String {
        let border_sgr = match (colors, self.border_fg) {
            (true, Some(fg)) => fg.sgr(3),
            _ => String::new(),
        };
        let width = lines.iter().map(|l| ansi::width(l)).max().unwrap_or(0);
        let edge = "─".repeat(width);
        let left = styled("│", &border_sgr);
        let right = styled("│", &border_sgr);
        let mut out = Vec::with_capacity(lines.len() + 2);
        out.push(styled(&format!("╭{edge}╮"), &border_sgr));
        out.extend(lines.iter().map(|line| format!("{left}{line}{right}")));
        out.push(styled(&format!("╰{edge}╯"), &border_sgr));
        out.join("\n")
    }
}

fn styled(text: &str, sgr: &str) -> String {
    if sgr.is_empty() { text.to_string() } else { format!("\x1b[{sgr}m{text}\x1b[0m") }
}

/// Places blocks side by side, top-aligned, padding each to its own width.
pub fn join_horizontal(blocks: &[&str]) -> String {
    let split: Vec<Vec<&str>> = blocks.iter().map(|b| b.split('\n').collect()).collect();
    let widths: Vec<usize> = blocks.iter().map(|b| ansi::block_width(b)).collect();
    let height = split.iter().map(Vec::len).max().unwrap_or(0);
    let mut rows = Vec::with_capacity(height);
    for row in 0..height {
        let mut line = String::new();
        for (lines, &width) in split.iter().zip(&widths) {
            let cell = lines.get(row).copied().unwrap_or("");
            line.push_str(cell);
            line.push_str(&" ".repeat(width - ansi::width(cell)));
        }
        rows.push(line);
    }
    rows.join("\n")
}

/// Stacks blocks, left-aligned and padded to the widest line.
pub fn join_vertical(blocks: &[&str]) -> String {
    let width = blocks.iter().map(|b| ansi::block_width(b)).max().unwrap_or(0);
    blocks
        .iter()
        .flat_map(|b| b.split('\n'))
        .map(|line| format!("{line}{}", " ".repeat(width - ansi::width(line))))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_bordered_padded_block() {
        let block = Style::new().width(8).height(3).padding(0, 1).border(Color::hex(0x334155)).render("ab\ncdef");
        assert_eq!(block, "╭────────╮\n│ ab     │\n│ cdef   │\n│        │\n╰────────╯");
    }

    #[test]
    fn wraps_to_width_minus_padding() {
        let block = Style::new().width(9).padding(1, 2).render("one two three");
        let lines: Vec<&str> = block.split('\n').collect();
        assert_eq!(lines, ["         ", "  one    ", "  two    ", "  three  ", "         "]);
    }

    #[test]
    fn inline_style_is_plain_without_color() {
        assert_eq!(Style::new().bold().fg(Color::hex(0x86efac)).render("x"), "x");
    }

    #[test]
    fn joins_blocks() {
        assert_eq!(join_horizontal(&["a\nbb", "c"]), "a c\nbb ");
        assert_eq!(join_vertical(&["abc", "d"]), "abc\nd  ");
    }
}
