//! Rendering: the dashboard panes, modal popups and text helpers.

use crate::ansi;
use crate::backend::is_running;
use crate::groups::{STANDALONE, group_summary};
use crate::model::{Focus, Model};
use crate::style::{Color, Style, join_horizontal, join_vertical};
use crate::update::action_progress_label;

pub const ACCENT: Color = Color::hex(0x86EFAC);
const MUTED: Color = Color::hex(0x94A3B8);
const RED: Color = Color::hex(0xFCA5A5);
const YELLOW: Color = Color::hex(0xFDE68A);
const PANEL: Color = Color::hex(0x334155);
const POPUP_BACKGROUND: Color = Color::hex(0x27272A);
const BRIGHT: Color = Color::hex(0xF8FAFC);

pub const TITLE: Style = Style::new().bold().fg(ACCENT);
pub const MUTED_STYLE: Style = Style::new().fg(MUTED);
pub const SELECTED: Style = Style::new().fg(ACCENT).bold();
pub const SELECTED_ROW: Style = Style::new().fg(BRIGHT).bg(Color::hex(0x14532D)).bold();
const RUNNING: Style = Style::new().fg(Color::hex(0x34D399));
const STOPPED: Style = Style::new().fg(MUTED);
pub const LOG_HEADING: Style = Style::new().fg(Color::hex(0x7DD3FC)).bold();
const LOG_SELECT: Style = Style::new().fg(BRIGHT).bg(PANEL);
pub const STATUS: Style = Style::new().fg(YELLOW);
pub const ERROR: Style = Style::new().fg(RED);

pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Screen row of a pane's first content line: the dashboard header occupies
/// row 0 and the pane's top border row 1.
pub const PANE_CONTENT_TOP: i32 = 2;

/// Pane geometry shared by the renderer and mouse hit-testing.
pub struct PaneLayout {
    pub left_width: usize,
    pub right_width: usize,
    pub body_height: usize,
    pub details_height: usize,
    pub logs_height: usize,
}

pub fn popup(width: usize, border: Color) -> Style {
    Style::new().width(width).padding(1, 2).border(border).bg(POPUP_BACKGROUND)
}

impl Model {
    pub fn pane_layout(&self) -> PaneLayout {
        let (width, height) = (self.width as i64, self.height as i64);
        let body_height = 5.max(height - 4);
        let left_width = 38.min(26.max(width / 3));
        let details_height = 10.min(2.max(body_height / 2));
        PaneLayout {
            left_width: left_width as usize,
            right_width: 20.max(width - left_width - 1) as usize,
            body_height: body_height as usize,
            details_height: details_height as usize,
            logs_height: 1.max(body_height - details_height - 2) as usize,
        }
    }

    pub fn view(&self) -> String {
        if self.width == 0 {
            return String::new();
        }
        if self.width < 60 || self.height < 16 {
            return truncate("terminal too small — resize to 60×16", self.width);
        }
        let dashboard = self.render_dashboard();
        let popup = if self.confirm_cleanup {
            self.render_cleanup_confirmation()
        } else if self.usage_overview {
            self.render_usage_overview()
        } else if self.confirm_delete {
            self.render_delete_confirmation()
        } else if self.action_menu {
            self.render_action_menu()
        } else {
            return dashboard;
        };
        overlay(self.width, self.height, &dashboard, &popup)
    }

    pub fn render_cleanup_confirmation(&self) -> String {
        let mut lines = vec![
            TITLE.render("clean up Docker storage?"),
            String::new(),
            "This removes unused images, stopped containers, unused networks,".to_string(),
            "build cache, and unused named or anonymous volumes.".to_string(),
            MUTED_STYLE.render("Volumes removed here cannot be recovered."),
            String::new(),
        ];
        lines.extend(self.cleanup_summary());
        lines.push(String::new());
        lines.extend(choice_lines(&["cancel", "clean up"], self.cleanup_choice));
        lines.push(String::new());
        lines.push(MUTED_STYLE.render("↑↓/j k select  enter confirm  y/n quick choice  esc cancel"));
        let width = 72.min(42.max(self.width as i64 - 4)) as usize;
        popup(width, RED).render(&lines.join("\n"))
    }

    pub fn render_delete_confirmation(&self) -> String {
        let name = self.delete_target().map_or("this container", |c| c.name.as_str());
        let mut lines = vec![
            TITLE.render("delete container?"),
            String::new(),
            format!("Remove {}?", sanitize_text(name)),
            MUTED_STYLE.render("This cannot be undone."),
            String::new(),
        ];
        lines.extend(choice_lines(&["cancel", "delete"], self.delete_choice));
        lines.push(String::new());
        lines.push(MUTED_STYLE.render("↑↓/j k select  enter confirm  y/n quick choice  esc cancel"));
        let width = 52.min(34.max(self.width as i64 - 4)) as usize;
        popup(width, RED).render(&lines.join("\n"))
    }

    fn render_dashboard(&self) -> String {
        let mut header = format!(
            "{}  {}",
            TITLE.render(crate::NAME),
            MUTED_STYLE.render(&format!("profile {}", sanitize_text(&self.current_profile_name())))
        );
        if let Some(p) = self.current_profile() {
            let running = is_running(&p.status);
            let (indicator, profile_status) = if running { ("●", RUNNING) } else { ("○", STATUS) };
            header += &format!(
                "  {}",
                profile_status.render(&format!("{indicator} {}", sanitize_text(&p.status).to_lowercase()))
            );
            header += &format!(
                "  {}",
                MUTED_STYLE.render(&format!(
                    "{} cpu · {} ram · {}",
                    p.cpus,
                    human_bytes(p.memory),
                    human_bytes(p.disk)
                ))
            );
            if let Some(remaining) = self.idle_remaining() {
                header += &format!(
                    "  {}",
                    STATUS.render(&format!("idle · auto-stop in {}", crate::autostop::format_countdown(remaining)))
                );
            }
        }

        let layout = self.pane_layout();
        let left = self.render_containers(layout.body_height, layout.left_width);
        let details = self.render_details(layout.details_height, layout.right_width);
        let logs = self.render_logs(layout.logs_height, layout.right_width);
        let right = join_vertical(&[&details, &logs]);
        let panes = join_horizontal(&[&left, &right]);

        let focus_name = if self.focus == Focus::Logs { "logs" } else { "containers" };
        let mut footer = MUTED_STYLE.render(&format!("focus: {focus_name}  / search  R running  ? actions  q quit"));
        if self.confirm_delete {
            footer = STATUS.render(&sanitize_text(&self.status));
        } else if let Some(err) = &self.err {
            footer = ERROR.render(&format!("{}: {}", sanitize_text(&self.status), sanitize_text(err)));
        } else if self.status != "ready" {
            footer = STATUS.render(&sanitize_text(&self.status));
        } else if !self.update_version.is_empty() {
            footer = STATUS.render(&format!(
                "update available: {} — run {} update",
                sanitize_text(&self.update_version),
                crate::NAME
            ));
        }
        if self.search_editing {
            footer = STATUS.render(&format!(
                "search: {}▏  enter apply · esc cancel · ctrl+u clear",
                sanitize_text(&self.search_query)
            ));
        }
        if self.log_search_editing {
            footer = STATUS.render(&format!(
                "log search: {}▏  enter apply · esc cancel · ctrl+u clear",
                sanitize_text(&self.log_query)
            ));
        }
        format!("{}\n{panes}\n{}", ansi::truncate(&header, self.width, ""), ansi::truncate(&footer, self.width, ""))
    }

    pub fn render_action_menu(&self) -> String {
        let items = self.action_menu_items();
        // The popup adds 11 chrome lines (border, padding, heading, footer)
        // plus one for the overflow marker; window the items so the overlay
        // never clips the footer, keeping the selected row visible.
        let (mut start, mut end) = (0, items.len());
        let visible = 3.max(self.height as i64 - 12) as usize;
        if items.len() > visible {
            start = self.action_index.saturating_sub(visible / 2).min(items.len() - visible);
            end = start + visible;
        }
        let mut lines =
            vec![TITLE.render("actions"), MUTED_STYLE.render("select an action and press enter"), String::new()];
        for (offset, item) in items[start..end].iter().enumerate() {
            let shortcut = format!("[{}]", item.shortcut);
            if !item.enabled {
                lines.push(MUTED_STYLE.render(&format!("  {}  {shortcut}", item.label)));
            } else if start + offset == self.action_index {
                lines.push(SELECTED_ROW.render(&format!("> {}  {shortcut}", item.label)));
            } else {
                lines.push(format!("  {}  {}", item.label, MUTED_STYLE.render(&shortcut)));
            }
        }
        let hidden = items.len() - (end - start);
        if hidden > 0 {
            lines.push(MUTED_STYLE.render(&format!("  ↑↓ {hidden} more")));
        }
        lines.extend([
            String::new(),
            LOG_HEADING.render("keyboard shortcuts"),
            MUTED_STYLE.render("↑↓/j k select  enter or shortcut key run  esc/? close"),
            MUTED_STYLE.render("[] profile  tab focus  end latest logs  q quit"),
        ]);
        let width = 78.min(38.max(self.width as i64 - 4)) as usize;
        popup(width, ACCENT).render(&lines.join("\n"))
    }

    pub fn render_containers(&self, height: usize, width: usize) -> String {
        let mut heading = "containers".to_string();
        if self.focus == Focus::Containers {
            heading = SELECTED.render(&format!("▸ {heading}"));
        }
        let mut lines = vec![format!("{heading}  {}", MUTED_STYLE.render(&self.containers.len().to_string()))];
        if self.filtering() {
            let filter = if self.running_only { "running only" } else { "all states" };
            lines[0] = format!(
                "{heading}  {}",
                MUTED_STYLE.render(&format!("{}/{}", self.matching_count(), self.containers.len()))
            );
            lines.push(
                MUTED_STYLE.render(&truncate(&format!("{filter} /{}", self.search_query), width.saturating_sub(4))),
            );
        }
        let items = self.list_items();
        if items.is_empty() {
            lines.push(String::new());
            if !self.containers.is_empty() && (!self.search_query.is_empty() || self.running_only) {
                lines.push(MUTED_STYLE.render("no matches"));
                lines.push(MUTED_STYLE.render("esc clears filters"));
            } else if self.current_profile().is_some_and(|p| !is_running(&p.status)) {
                lines.push(MUTED_STYLE.render("colima is stopped"));
                lines.push(STATUS.render("press s to start"));
            } else {
                lines.push(MUTED_STYLE.render("no containers"));
            }
            return self.render_pane(&lines, width, height, self.focus == Focus::Containers);
        }
        let (header_lines, start) = self.container_list_window(height);
        let row_count = 1.max(height as i64 - header_lines as i64) as usize;
        let end = items.len().min(start + row_count);
        let row_width = 8.max(width as i64 - 4);
        for (i, item) in items.iter().enumerate().take(end).skip(start) {
            if item.group_header {
                let group = self.selected_group_by_name(&item.group);
                let marker = if self.is_expanded(&item.group) { "▼" } else { "▶" };
                let summary = group_summary(&group, &self.containers);
                let summary_width = (summary.len() as i64).min(4.max(row_width - 9));
                let name_width = 18.min(4.max(row_width - summary_width - 5)) as usize;
                let summary = truncate(&summary, summary_width as usize);
                let name = middle_truncate(&group.name, name_width);
                if i == self.container_index {
                    lines.push(SELECTED_ROW.render(&format!("> {marker} {name:<name_width$} {summary}")));
                } else {
                    lines.push(format!("  {} {name} {}", TITLE.render(marker), MUTED_STYLE.render(&summary)));
                }
                continue;
            }
            let c = &self.containers[item.container_index];
            let indent = if item.group == STANDALONE { "" } else { "  " };
            let name_width = 18.min(10.max(row_width - 11 - indent.len() as i64)) as usize;
            let status_width = 4.max(row_width - name_width as i64 - 5 - indent.len() as i64) as usize;
            let mut marker = if c.state == "running" { "●" } else { "○" };
            let mut container_status = status_label(&c.status, status_width);
            let action = self.active_container_action(&c.id);
            if let Some(action) = action {
                marker = SPINNER_FRAMES[self.spinner_frame];
                container_status = action_progress_label(&action.label);
            }
            let name = middle_truncate(c.list_name(), name_width);
            if i == self.container_index {
                lines.push(SELECTED_ROW.render(&format!("> {indent}{marker} {name:<name_width$} {container_status}")));
            } else {
                let style = match (action.is_some(), c.state == "running") {
                    (true, _) => STATUS,
                    (false, true) => RUNNING,
                    (false, false) => STOPPED,
                };
                lines.push(format!(
                    "  {indent}{} {name:<name_width$} {}",
                    style.render(marker),
                    style.render(&container_status)
                ));
            }
        }
        self.render_pane(&lines, width, height, self.focus == Focus::Containers)
    }

    fn filtering(&self) -> bool {
        !self.search_query.is_empty() || self.running_only || self.search_editing
    }

    /// The containers pane's header row count and first visible list item,
    /// mirroring render_containers so mouse clicks land on the right item.
    pub fn container_list_window(&self, height: usize) -> (usize, usize) {
        let header_lines = if self.filtering() { 2 } else { 1 };
        let items = self.list_items().len();
        let row_count = 1.max(height as i64 - header_lines as i64) as usize;
        let start = (self.container_index + 1).saturating_sub(row_count);
        (header_lines, start.min(items.saturating_sub(row_count)))
    }

    pub fn render_details(&self, height: usize, width: usize) -> String {
        let mut lines = vec!["details".to_string()];
        match self.selected_container() {
            None => match self.selected_group() {
                Some(group) => lines.extend([
                    String::new(),
                    TITLE.render(&sanitize_text(&group.name)),
                    String::new(),
                    format!("services {}", group.indices.len()),
                    format!("status   {}", group_summary(&group, &self.containers)),
                ]),
                None => lines.extend([String::new(), MUTED_STYLE.render("select a container")]),
            },
            Some(c) => {
                let value_width = 10.max(width as i64 - 10) as usize;
                lines.push(TITLE.render(&truncate(&c.name, 10.max(width as i64 - 2) as usize)));
                lines.push(format!("state   {}", truncate(&format!("{} · {}", c.state, c.status), value_width)));
                lines.extend(self.resource_lines(width.saturating_sub(4)));
                lines.extend([
                    format!("image   {}", truncate(&c.image, value_width)),
                    format!("id      {}", truncate(&c.id, value_width)),
                    format!("command {}", truncate(&c.command, value_width)),
                    format!("ports   {}", truncate(&c.ports, value_width)),
                ]);
            }
        }
        // Keep long metadata and metrics from wrapping into neighboring panes.
        let lines = fit_rows(lines, height, width);
        self.render_pane(&lines, width, height, false)
    }

    pub fn render_logs(&self, height: usize, width: usize) -> String {
        let mut heading = "logs".to_string();
        if self.focus == Focus::Logs {
            heading = SELECTED.render(&format!("▸ {heading}"));
        }
        let state = if self.follow { "following · f pause" } else { "paused · f resume" };
        let wrap = if self.log_wrap { "w trim" } else { "w wrap" };
        let mut lines = vec![format!("{heading}  {state} · {wrap}")];
        if !self.log_query.is_empty() {
            lines.push(MUTED_STYLE.render(&truncate(
                &format!("/{} · {} matches", self.log_query, self.filtered_logs().len()),
                width.saturating_sub(4),
            )));
        }
        if self.logs_truncated || self.partial_trimmed {
            lines.push(MUTED_STYLE.render("logs truncated (memory limit)"));
        }
        if self.selected_container().is_none() {
            lines.extend([String::new(), MUTED_STYLE.render("select a service")]);
        } else if self.logs.is_empty() {
            lines.push(MUTED_STYLE.render("no logs"));
        } else {
            if self.filtered_logs().is_empty() {
                lines.push(MUTED_STYLE.render("no matching logs"));
            }
            let count = (height as i64 - lines.len() as i64 - 1).max(0) as usize;
            let (rows, indices) = self.log_rows_indexed(count, 1.max(width as i64 - 4) as usize);
            let (lo, hi) = (self.log_sel_start.min(self.log_sel_end), self.log_sel_start.max(self.log_sel_end));
            for (row, index) in rows.into_iter().zip(indices) {
                if self.log_sel_active && (lo..=hi).contains(&index) {
                    lines.push(LOG_SELECT.render(&row));
                } else {
                    lines.push(row);
                }
            }
        }
        // Lipgloss heights are minimums and do not clip wrapped text; bound
        // every row and the row count so long logs cannot grow the pane.
        let lines = fit_rows(lines, height, width);
        self.render_pane(&lines, width, height, self.focus == Focus::Logs)
    }

    /// Mirrors the non-log heading rows render_logs draws, so mouse
    /// hit-testing can find the first log row.
    pub fn log_header_lines(&self) -> usize {
        1 + usize::from(!self.log_query.is_empty()) + usize::from(self.logs_truncated || self.partial_trimmed)
    }

    fn render_pane(&self, lines: &[String], width: usize, height: usize, focused: bool) -> String {
        let border = if focused { ACCENT } else { PANEL };
        Style::new()
            .width(width.saturating_sub(2).max(1))
            .height(height)
            .padding(0, 1)
            .border(border)
            .render(&lines.join("\n"))
    }
}

fn choice_lines(options: &[&str], selected: usize) -> Vec<String> {
    options
        .iter()
        .enumerate()
        .map(
            |(index, option)| {
                if index == selected { SELECTED_ROW.render(&format!("> {option}")) } else { format!("  {option}") }
            },
        )
        .collect()
}

fn fit_rows(lines: Vec<String>, height: usize, width: usize) -> Vec<String> {
    let width = width.saturating_sub(4).max(1);
    lines.into_iter().take(height).map(|line| ansi::truncate(&line, width, "")).collect()
}

/// Centers `foreground` over `background`, padding both to the screen size.
pub fn overlay(width: usize, height: usize, background: &str, foreground: &str) -> String {
    let background: Vec<&str> = background.split('\n').collect();
    let foreground: Vec<&str> = foreground.split('\n').collect();
    let fg_width = foreground.iter().map(|l| ansi::width(l)).max().unwrap_or(0);
    let x = width.saturating_sub(fg_width) / 2;
    let y = height.saturating_sub(foreground.len()) / 2;
    (0..height)
        .map(|row| {
            let mut line = background.get(row).map_or_else(String::new, |l| ansi::truncate(l, width, ""));
            line.push_str(&" ".repeat(width.saturating_sub(ansi::width(&line))));
            if row >= y && row < y + foreground.len() {
                let mut popup = ansi::truncate(foreground[row - y], fg_width, "");
                popup.push_str(&" ".repeat(fg_width - ansi::width(&popup)));
                line = format!("{}{popup}{}", ansi::cut(&line, 0, x), ansi::cut(&line, x + fg_width, width));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn human_bytes(value: i64) -> String {
    if value <= 0 {
        return "0b".to_string();
    }
    const UNITS: [&str; 5] = ["b", "k", "m", "g", "t"];
    let mut amount = value as f64;
    let mut i = 0;
    while amount >= 1024.0 && i < UNITS.len() - 1 {
        amount /= 1024.0;
        i += 1;
    }
    if amount >= 10.0 || i == 0 { format!("{amount:.0}{}", UNITS[i]) } else { format!("{amount:.1}{}", UNITS[i]) }
}

pub fn truncate(value: &str, width: usize) -> String {
    let value = sanitize_text(value);
    if width < 4 || ansi::width(&value) <= width {
        return value;
    }
    ansi::truncate(&value, width, "...")
}

pub fn middle_truncate(value: &str, width: usize) -> String {
    let value = sanitize_text(value);
    let value_width = ansi::width(&value);
    if width < 4 || value_width <= width {
        return value;
    }
    let left = (width - 3).div_ceil(2);
    let right = width - 3 - left;
    format!(
        "{}...{}",
        ansi::truncate(&value, left, ""),
        ansi::cut(&value, value_width.saturating_sub(right), value_width)
    )
}

pub fn status_label(value: &str, width: usize) -> String {
    let value = sanitize_text(value);
    match value.split_whitespace().next() {
        Some(first) if width >= 4 && first.len() <= width => first.to_string(),
        _ => truncate(&value, width),
    }
}

/// Strips terminal escape sequences (ESC and C1 introduced) and replaces
/// other control characters with spaces, so container metadata and logs
/// cannot drive the terminal.
pub fn sanitize_text(value: &str) -> String {
    if !value.chars().any(char::is_control) {
        return value.to_string();
    }
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\x1b' => i = skip_escape(&chars, i),
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => i = skip_string_control(&chars, i + 1),
            '\u{9b}' => i = skip_csi(&chars, i + 1),
            '\u{9c}' => i += 1,
            c => {
                out.push(if c.is_control() { ' ' } else { c });
                i += 1;
            }
        }
    }
    out
}

fn skip_escape(chars: &[char], start: usize) -> usize {
    let mut i = start + 1;
    let Some(&kind) = chars.get(i) else { return i };
    match kind {
        '[' => return skip_csi(chars, i + 1),
        ']' | 'P' | '^' | '_' | 'X' => return skip_string_control(chars, i + 1),
        _ => {}
    }
    while i < chars.len() && ('\x20'..='\x2f').contains(&chars[i]) {
        i += 1;
    }
    if i < chars.len() && ('\x30'..='\x7e').contains(&chars[i]) { i + 1 } else { i }
}

fn skip_csi(chars: &[char], start: usize) -> usize {
    (start..chars.len()).find(|&i| ('\x40'..='\x7e').contains(&chars[i])).map_or(chars.len(), |i| i + 1)
}

fn skip_string_control(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() {
        match chars[i] {
            '\x07' | '\u{9c}' => return i + 1,
            '\x1b' if chars.get(i + 1) == Some(&'\\') => return i + 2,
            _ => i += 1,
        }
    }
    chars.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::{block_height, block_width};
    use crate::model::{ActiveAction, Container, Msg};
    use crate::testutil::{container, model, profile};

    #[test]
    fn view_fits_small_terminals() {
        for (width, height) in [(80, 24), (120, 40), (40, 12), (20, 5)] {
            let mut m = model();
            (m.width, m.height, m.status) = (width, height, "ready".into());
            let view = m.view();
            assert!(block_width(&view) <= width && block_height(&view) <= height, "terminal {width}x{height}");
        }
    }

    #[test]
    fn active_container_shows_progress() {
        let mut m = model();
        (m.width, m.height, m.status) = (100, 24, "stopping api".into());
        m.containers = vec![Container { status: "Up".into(), ..container("api-id", "api", "running") }];
        m.active_actions.insert(1, ActiveAction { container_id: "api-id".into(), label: "stop".into() });
        let view = m.view();
        assert!(view.contains("stopping…") && view.contains(SPINNER_FRAMES[0]), "{view}");
        assert!(m.update(Msg::SpinnerTick).is_some());
        assert_eq!(m.spinner_frame, 1);
    }

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(0), "0b");
        assert_eq!(human_bytes(1024), "1.0k");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0m");
        assert_eq!(human_bytes(60 << 30), "60g");
        assert_eq!(human_bytes(500), "500b");
    }

    #[test]
    fn container_pane_fits_height() {
        let mut m = model();
        m.containers = vec![Container { status: "Up".into(), ..container("", "colimui-test", "running") }; 51];
        m.container_index = 50;
        assert_eq!(block_height(&m.render_containers(20, 38)), 22);
    }

    #[test]
    fn stopped_profile_shows_start_hint() {
        let mut m = model();
        m.profiles = vec![profile("default", "Stopped")];
        let view = m.render_containers(10, 38);
        assert!(view.contains("colima is stopped") && view.contains("press s to start"), "{view}");
    }

    #[test]
    fn action_menu_shows_shortcuts() {
        let mut m = model();
        (m.width, m.height, m.status, m.action_menu) = (100, 36, "ready".into(), true);
        m.containers = vec![container("", "api", "running")];
        let view = m.view();
        for text in [
            "colimuir",
            "containers",
            "actions",
            "stop api",
            "restart api",
            "delete api",
            "keyboard shortcuts",
            "enter or shortcut key run",
            "[] profile",
        ] {
            assert!(view.contains(text), "action menu is missing {text:?}: {view}");
        }
    }

    #[test]
    fn action_menu_windows_items_to_height() {
        let mut m = model();
        (m.width, m.height, m.status, m.action_menu) = (100, 20, "ready".into(), true);
        m.containers = vec![container("", "api", "running")];
        let view = m.view();
        assert!(view.contains("more") && view.contains("[] profile"), "{view}");
        m.action_index = m.action_menu_items().len() - 1;
        assert!(m.view().contains("> show running only"), "selected last item is not visible");
    }

    #[test]
    fn details_pane_preserves_border() {
        let mut m = model();
        m.containers =
            vec![Container { status: "Up".into(), image: "alpine".into(), ..container("abc", "test", "running") }];
        m.logs = vec![String::new(); 200].into();
        let view = m.render_details(20, 80);
        assert_eq!(block_height(&view), 22);
        assert!(view.contains('╰'), "details pane is missing its bottom border");
    }

    #[test]
    fn details_pane_is_not_selectable() {
        let mut m = model();
        m.focus = Focus::Logs;
        m.containers = vec![container("abc", "test", "running")];
        assert!(!m.render_details(10, 80).contains("▸ details"));
    }

    #[test]
    fn logs_pane_shows_focus() {
        let mut m = model();
        m.focus = Focus::Logs;
        m.containers = vec![container("abc", "test", "running")];
        m.logs = ["hello".to_string()].into();
        assert!(m.render_logs(8, 80).contains("▸ logs"));
    }

    #[test]
    fn logs_pane_clips_long_rows_to_its_allocated_size() {
        let mut m = model();
        m.focus = Focus::Logs;
        m.containers = vec![container("postgres", "postgres", "running")];
        m.logs = [
            "2026-09-05 16:32:24.400 UTC [1] LOG: PostgreSQL startup ".repeat(4),
            "database system is ready to accept connections ".repeat(4),
        ]
        .into();
        let view = m.render_logs(10, 80);
        assert_eq!(block_height(&view), 12);
        assert!(block_width(&view) <= 80);
    }

    #[test]
    fn log_wrapping_uses_bounded_terminal_rows() {
        let long_line = format!("postgres 数据库 startup {}", "connection-ready ".repeat(8));
        let mut m = model();
        m.containers = vec![container("postgres", "postgres", "running")];
        m.logs = [long_line.clone()].into();
        m.log_wrap = true;
        let (rows, _) = m.log_rows_indexed(6, 20);
        assert!((2..=6).contains(&rows.len()), "wrapped rows = {rows:?}");
        assert!(rows.iter().all(|row| ansi::width(row) <= 20));
        assert_eq!(block_height(&m.render_logs(10, 80)), 12);
        m.log_wrap = false;
        let (rows, _) = m.log_rows_indexed(6, 20);
        assert!(rows.len() == 1 && ansi::width(&rows[0]) <= 20 && rows[0] != long_line);
    }

    #[test]
    fn compose_group_fits_narrow_pane() {
        let mut m = model();
        m.containers = vec![Container {
            compose_project: "ides".into(),
            compose_service: "postgres".into(),
            status: "Up".into(),
            ..container("1", "ides-postgres-1", "running")
        }];
        assert_eq!(block_height(&m.render_containers(20, 26)), 22);
    }

    #[test]
    fn middle_truncate_preserves_suffix() {
        let got = middle_truncate("colimui-test-01", 10);
        assert!(got.len() == 10 && got.ends_with("01"), "{got}");
        assert_eq!(status_label("Up 31 minutes", 6), "Up");
    }

    #[test]
    fn sanitize_text_strips_terminal_controls() {
        let input = crate::gocompat::decode_bytes(
            b"safe\x1b[31mred\x1b[2J \x1b]0;title\x07\x1bPdata\x1b\\\x9b?25l next\x1b#8\nlast",
        );
        assert_eq!(sanitize_text(&input), "safered  next last");
    }

    #[test]
    fn render_sanitizes_container_text_and_logs() {
        let mut m = model();
        m.containers = vec![Container {
            id: "id".into(),
            name: "container\x1b]0;title\x07".into(),
            image: "image\x1b[2J".into(),
            state: "running".into(),
            status: "Up\x1b[?25l".into(),
            command: "command\x1b#8".into(),
            ports: "ports\u{9b}?25l".into(),
            ..Container::default()
        }];
        m.logs = ["log\x1b]52;c;clipboard\x07".to_string()].into();
        let details = m.render_details(12, 80);
        assert!(!details.contains("\x1b]0;title") && !details.contains("\x1b[2J"), "{details:?}");
        assert!(!m.render_logs(8, 80).contains("\x1b]52;c;clipboard"));
    }

    #[test]
    fn update_notice_is_shown() {
        let mut m = model();
        (m.width, m.height, m.status) = (100, 24, "ready".into());
        assert!(m.update(Msg::UpdateCheck("v0.0.2".into())).is_none());
        let view = m.view();
        assert!(view.contains("update available: v0.0.2") && view.contains("run colimuir update"), "{view}");
    }
}
