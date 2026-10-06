//! ANSI-aware measuring and slicing of styled terminal strings, following
//! charmbracelet/x/ansi so rendered rows keep the exact widths the layout
//! math expects.

use unicode_width::UnicodeWidthChar;

/// The reset cellbuf.Wrap writes before a line break inside a styled run.
const WRAP_RESET: &str = "\x1b[m";

enum Token<'a> {
    Sequence(&'a str),
    Char(char),
}

fn tokens(s: &str) -> impl Iterator<Item = Token<'_>> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let rest = &s[pos..];
        let c = rest.chars().next()?;
        if c == '\x1b' {
            let len = sequence_len(rest);
            pos += len;
            Some(Token::Sequence(&rest[..len]))
        } else {
            pos += c.len_utf8();
            Some(Token::Char(c))
        }
    })
}

/// Length of the escape sequence at the start of `s`, which begins with ESC.
fn sequence_len(s: &str) -> usize {
    let b = s.as_bytes();
    let Some(&kind) = b.get(1) else { return 1 };
    match kind {
        b'[' => b[2..].iter().position(|c| (0x40..=0x7e).contains(c)).map_or(b.len(), |i| i + 3),
        b']' | b'P' | b'^' | b'_' | b'X' => {
            let mut i = 2;
            while i < b.len() {
                if b[i] == 0x07 {
                    return i + 1;
                }
                if b[i] == 0x1b && b.get(i + 1) == Some(&b'\\') {
                    return i + 2;
                }
                i += 1;
            }
            b.len()
        }
        _ => {
            let mut i = 1;
            while i < b.len() && (0x20..=0x2f).contains(&b[i]) {
                i += 1;
            }
            if i < b.len() && (0x30..=0x7e).contains(&b[i]) { i + 1 } else { i }
        }
    }
}

fn char_width(c: char) -> usize {
    if c.is_control() { 0 } else { c.width().unwrap_or(0) }
}

#[cfg(test)]
pub fn strip(s: &str) -> String {
    tokens(s)
        .filter_map(|t| match t {
            Token::Char(c) => Some(c),
            Token::Sequence(_) => None,
        })
        .collect()
}

/// Display width of one line, ignoring escape sequences.
pub fn width(s: &str) -> usize {
    tokens(s)
        .map(|t| match t {
            Token::Char(c) => char_width(c),
            Token::Sequence(_) => 0,
        })
        .sum()
}

/// Width of the widest line, like lipgloss.Width.
pub fn block_width(s: &str) -> usize {
    s.split('\n').map(width).max().unwrap_or(0)
}

/// Line count, like lipgloss.Height.
#[cfg(test)]
pub fn block_height(s: &str) -> usize {
    s.matches('\n').count() + 1
}

/// Shortens `s` to `length` cells including `tail`, keeping every escape
/// sequence so styles still reset.
pub fn truncate(s: &str, length: usize, tail: &str) -> String {
    if width(s) <= length {
        return s.to_string();
    }
    let Some(length) = length.checked_sub(width(tail)) else {
        return String::new();
    };
    let mut out = String::with_capacity(s.len());
    let mut current = 0;
    let mut ignoring = false;
    for token in tokens(s) {
        match token {
            Token::Sequence(seq) => out.push_str(seq),
            Token::Char(c) if c.is_control() => out.push(c),
            Token::Char(c) => {
                let w = char_width(c);
                if !ignoring && current + w > length {
                    ignoring = true;
                    out.push_str(tail);
                }
                if !ignoring {
                    out.push(c);
                    current += w;
                }
            }
        }
    }
    out
}

/// Drops the first `n` cells, keeping escape sequences from the dropped part.
pub fn truncate_left(s: &str, n: usize) -> String {
    if n == 0 {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut current = 0;
    let mut ignoring = true;
    for token in tokens(s) {
        match token {
            Token::Sequence(seq) => out.push_str(seq),
            Token::Char(c) if !ignoring || c.is_control() => out.push(c),
            Token::Char(c) => {
                current += char_width(c);
                if current > n {
                    ignoring = false;
                    out.push(c);
                }
            }
        }
    }
    out
}

/// The cells in `[left, right)`.
pub fn cut(s: &str, left: usize, right: usize) -> String {
    if right <= left {
        return String::new();
    }
    truncate_left(&truncate(s, right, ""), left)
}

/// Breaks unstyled text into rows of at most `limit` cells, preserving spaces.
pub fn hardwrap(s: &str, limit: usize) -> String {
    if limit < 1 {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + s.len() / limit.max(1));
    let mut current = 0;
    for c in s.chars() {
        if c == '\n' {
            out.push('\n');
            current = 0;
            continue;
        }
        let w = char_width(c);
        if current + w > limit {
            out.push('\n');
            current = 0;
        }
        out.push(c);
        current += w;
    }
    out
}

/// Word-wraps styled text at `limit` cells like lipgloss block rendering:
/// breaks at spaces and hyphens, hard-breaks long words, and carries the
/// active style across the inserted line breaks.
pub fn wrap(s: &str, limit: usize) -> String {
    if limit < 1 {
        return s.to_string();
    }
    let mut w = Wrapper { limit, ..Wrapper::default() };
    for token in tokens(s) {
        match token {
            Token::Sequence(seq) => {
                w.track_style(seq);
                w.word.push_str(seq);
            }
            Token::Char('\n') => {
                w.flush_trailing_space();
                w.add_word();
                w.add_newline();
            }
            Token::Char(c) if c.is_ascii() => {
                if c.is_ascii_whitespace() || c == '\x0b' {
                    w.add_word();
                    w.space.push(c);
                } else if c == '-' {
                    w.add_space();
                    if w.current + w.word_len >= limit {
                        w.word.push(c);
                        w.word_len += 1;
                    } else {
                        w.add_word();
                        w.buf.push(c);
                        w.current += 1;
                    }
                } else {
                    if w.current == limit {
                        w.add_newline();
                    }
                    w.word.push(c);
                    w.word_len += 1;
                    if w.word_len == limit {
                        w.add_word();
                    }
                    if w.current + w.word_len + w.space.len() > limit {
                        w.add_newline();
                    }
                }
            }
            Token::Char(c) => {
                if c.is_whitespace() && c != '\u{a0}' {
                    w.add_word();
                    w.space.push(c);
                    continue;
                }
                let cw = char_width(c);
                if w.word_len + cw > limit {
                    w.add_word();
                }
                w.word.push(c);
                w.word_len += cw;
                if w.current + w.word_len + w.space.len() > limit {
                    w.add_newline();
                }
            }
        }
    }
    w.flush_trailing_space();
    w.add_word();
    w.buf
}

#[derive(Default)]
struct Wrapper {
    limit: usize,
    buf: String,
    word: String,
    space: String,
    current: usize,
    word_len: usize,
    style: Vec<String>,
    word_style: Vec<String>,
}

impl Wrapper {
    fn track_style(&mut self, seq: &str) {
        let Some(params) = seq.strip_prefix("\x1b[").and_then(|p| p.strip_suffix('m')) else {
            return;
        };
        if params.is_empty() || params == "0" {
            self.style.clear();
        } else if let Some(rest) = params.strip_prefix("0;") {
            self.style.clear();
            self.style.push(format!("\x1b[{rest}m"));
        } else {
            self.style.push(seq.to_string());
        }
    }

    fn add_space(&mut self) {
        self.current += self.space.len();
        self.buf.push_str(&self.space);
        self.space.clear();
    }

    fn add_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        self.word_style.clone_from(&self.style);
        self.add_space();
        self.current += self.word_len;
        self.buf.push_str(&self.word);
        self.word.clear();
        self.word_len = 0;
    }

    fn add_newline(&mut self) {
        let styled = !self.word_style.is_empty();
        if styled {
            self.buf.push_str(WRAP_RESET);
        }
        self.buf.push('\n');
        if styled {
            self.buf.push_str(&self.word_style.concat());
        }
        self.current = 0;
        self.space.clear();
    }

    /// Keeps trailing spaces that still fit before a break; drops them otherwise.
    fn flush_trailing_space(&mut self) {
        if self.word_len == 0 {
            if self.current + self.space.len() > self.limit {
                self.current = 0;
            } else {
                self.buf.push_str(&self.space);
            }
            self.space.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: &str = "\x1b[31m";
    const RESET: &str = "\x1b[0m";

    #[test]
    fn measures_without_escapes() {
        assert_eq!(width(&format!("{RED}abc{RESET}")), 3);
        assert_eq!(width("数据库"), 6);
        assert_eq!(block_width("ab\nabcd"), 4);
        assert_eq!(block_height("a\nb\nc"), 3);
        assert_eq!(strip(&format!("{RED}x{RESET}y")), "xy");
    }

    #[test]
    fn truncates_keeping_escapes() {
        assert_eq!(truncate("hello world", 8, "..."), "hello...");
        assert_eq!(truncate("short", 8, "..."), "short");
        assert_eq!(truncate(&format!("{RED}hello{RESET}"), 3, ""), format!("{RED}hel{RESET}"));
        assert_eq!(truncate("数据库", 3, ""), "数");
        assert_eq!(truncate("abc", 1, "..."), "");
    }

    #[test]
    fn cuts_cell_ranges() {
        assert_eq!(cut("abcdefgh", 2, 5), "cde");
        assert_eq!(cut(&format!("{RED}abcdef{RESET}"), 1, 3), format!("{RED}bc{RESET}"));
        assert_eq!(truncate_left("abcdef", 4), "ef");
    }

    #[test]
    fn hardwraps_rows() {
        assert_eq!(hardwrap("abcdefg", 3), "abc\ndef\ng");
        assert_eq!(hardwrap("ab数据", 3), "ab\n数\n据");
    }

    #[test]
    fn word_wraps_at_spaces_and_hard_breaks_long_words() {
        assert_eq!(wrap("the quick brown fox", 10), "the quick\nbrown fox");
        assert_eq!(wrap("abcdefghijkl", 5), "abcde\nfghij\nkl");
        assert_eq!(wrap("well-known words", 6), "well-\nknown\nwords");
        assert_eq!(wrap("fits", 10), "fits");
    }

    #[test]
    fn word_wrap_carries_style_across_breaks() {
        let wrapped = wrap(&format!("{RED}aaa bbb{RESET}"), 4);
        assert_eq!(wrapped, format!("{RED}aaa{WRAP_RESET}\n{RED}bbb{RESET}"));
    }
}
