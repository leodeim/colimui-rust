//! Mouse handling: wheel scrolling, click selection and drag-to-copy logs.

use crate::model::{Cmd, Focus, Model};
use crate::tea::{Mouse, MouseAction, MouseButton};
use crate::view::{PANE_CONTENT_TOP, PaneLayout};

impl Model {
    pub fn mouse(&mut self, event: Mouse) -> Option<Cmd> {
        if self.usage_overview || self.confirm_cleanup || self.confirm_delete || self.action_menu || self.width == 0 {
            return None;
        }
        if event.is_wheel() {
            if event.action != MouseAction::Press {
                return None;
            }
            return self.wheel(event);
        }
        match event.action {
            MouseAction::Press if event.button == MouseButton::Left => self.left_press(event),
            MouseAction::Motion if self.log_selecting => {
                self.extend_log_selection(event);
                None
            }
            MouseAction::Release if self.log_selecting => self.finish_log_selection(),
            _ => None,
        }
    }

    fn wheel(&mut self, event: Mouse) -> Option<Cmd> {
        if (event.x as i64) < self.pane_layout().left_width as i64 {
            let item_count = self.list_items().len();
            if event.button == MouseButton::WheelUp && self.container_index > 0 {
                self.container_index -= 1;
                return self.reload_selected_logs(false);
            }
            if event.button == MouseButton::WheelDown && self.container_index + 1 < item_count {
                self.container_index += 1;
                return self.reload_selected_logs(false);
            }
            return None;
        }
        self.focus = Focus::Logs;
        self.pause_logs();
        match event.button {
            MouseButton::WheelUp => self.log_scroll = self.filtered_logs().len().min(self.log_scroll + 3),
            MouseButton::WheelDown => self.log_scroll = self.log_scroll.saturating_sub(3),
            _ => {}
        }
        None
    }

    fn left_press(&mut self, event: Mouse) -> Option<Cmd> {
        self.clear_log_selection();
        let layout = self.pane_layout();
        if (event.x as i64) < layout.left_width as i64 {
            self.focus = Focus::Containers;
            let (header_lines, start) = self.container_list_window(layout.body_height);
            let items = self.list_items();
            let first_row = PANE_CONTENT_TOP as i64 + header_lines as i64;
            let index = start as i64 + event.y as i64 - first_row;
            if (event.y as i64) < first_row || index < 0 || index >= items.len() as i64 {
                return None;
            }
            let index = index as usize;
            if index == self.container_index {
                if items[index].group_header {
                    self.toggle_selected_group();
                }
                return None;
            }
            self.container_index = index;
            return self.reload_selected_logs(false);
        }
        let content_top = log_content_top(&layout);
        if (event.y as i64) < content_top {
            return None;
        }
        self.focus = Focus::Logs;
        let (rows, indices) = self.log_rows_indexed(self.log_row_capacity(&layout), log_row_width(&layout));
        let row = event.y as i64 - content_top - self.log_header_lines() as i64;
        if row < 0 || row >= rows.len() as i64 {
            return None;
        }
        self.pause_logs();
        self.log_selecting = true;
        self.log_sel_active = true;
        self.log_sel_dragged = false;
        self.log_sel_start = indices[row as usize];
        self.log_sel_end = indices[row as usize];
        None
    }

    fn extend_log_selection(&mut self, event: Mouse) {
        let layout = self.pane_layout();
        let (_, indices) = self.log_rows_indexed(self.log_row_capacity(&layout), log_row_width(&layout));
        if indices.is_empty() {
            return;
        }
        let row = event.y as i64 - log_content_top(&layout) - self.log_header_lines() as i64;
        let row = row.clamp(0, indices.len() as i64 - 1) as usize;
        self.log_sel_end = indices[row];
        self.log_sel_dragged = true;
    }

    /// A plain click only focuses the pane; copying requires a drag so that
    /// clicking around never clobbers the clipboard.
    fn finish_log_selection(&mut self) -> Option<Cmd> {
        self.log_selecting = false;
        if !self.log_sel_dragged {
            self.log_sel_active = false;
            return None;
        }
        let (lo, hi) = (self.log_sel_start.min(self.log_sel_end), self.log_sel_start.max(self.log_sel_end));
        let filtered = self.filtered_logs();
        if filtered.is_empty() || lo >= filtered.len() {
            self.log_sel_active = false;
            return None;
        }
        let hi = hi.min(filtered.len() - 1);
        let lines = self.log_text_lines(&filtered[lo..=hi]);
        let label = crate::clipboard::count_label(lines.len(), "log line");
        self.copy_text(&lines.join("\n"), &label)
    }

    fn log_row_capacity(&self, layout: &PaneLayout) -> usize {
        layout.logs_height.saturating_sub(self.log_header_lines() + 1)
    }
}

/// Screen row of the log pane's first content line (its heading), below the
/// details pane and the log pane's own border.
fn log_content_top(layout: &PaneLayout) -> i64 {
    layout.details_height as i64 + 4
}

fn log_row_width(layout: &PaneLayout) -> usize {
    layout.right_width.saturating_sub(4).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Container, Msg};
    use crate::testutil::{FakeBackend, container, model, model_with, profile, stub_clipboard};

    fn left(action: MouseAction, x: i32, y: i32) -> Msg {
        Msg::Mouse(Mouse { x, y, action, button: MouseButton::Left })
    }

    fn wheel(button: MouseButton, x: i32) -> Msg {
        Msg::Mouse(Mouse { x, y: 0, action: MouseAction::Press, button })
    }

    #[test]
    fn click_selects_container_and_reloads_logs() {
        let backend = FakeBackend::new();
        let mut m = model_with(&backend);
        (m.width, m.height, m.focus) = (120, 24, Focus::Logs);
        m.profiles = vec![profile("default", "Running")];
        m.containers = vec![container("one", "one", "running"), container("two", "two", "running")];
        m.update(left(MouseAction::Press, 2, 4));
        assert_eq!((m.container_index, m.focus), (1, Focus::Containers));
        assert_eq!(backend.state().log_id, "two");
        m.update(left(MouseAction::Press, 2, 12));
        assert_eq!(m.container_index, 1, "click below the list moved the selection");
    }

    #[test]
    fn click_on_selected_group_header_toggles() {
        let mut m = model();
        (m.width, m.height) = (120, 24);
        m.profiles = vec![profile("default", "Running")];
        m.containers = vec![Container {
            compose_project: "app".into(),
            compose_service: "web".into(),
            ..container("one", "one", "running")
        }];
        m.update(left(MouseAction::Press, 2, 3));
        assert!(!m.is_expanded("app"), "click on the selected group header did not collapse it");
        m.update(left(MouseAction::Press, 2, 3));
        assert!(m.is_expanded("app"), "second click did not expand the group again");
    }

    fn log_model() -> Model {
        let mut m = model();
        (m.width, m.height) = (120, 24);
        m.profiles = vec![profile("default", "Running")];
        m.containers = vec![container("one", "one", "running")];
        m
    }

    #[test]
    fn log_drag_selection_copies_lines() {
        let (clipboard, copied) = stub_clipboard();
        let mut m = log_model();
        m.clipboard = clipboard;
        m.follow = true;
        m.logs = ["2024-01-01T00:00:00Z alpha", "2024-01-01T00:00:01Z beta", "2024-01-01T00:00:02Z gamma"]
            .map(String::from)
            .into();
        m.update(left(MouseAction::Press, 60, 15));
        assert!(m.log_selecting && !m.follow && m.focus == Focus::Logs);
        m.update(left(MouseAction::Motion, 60, 16));
        m.update(left(MouseAction::Release, 60, 16));
        assert_eq!(*copied.lock().unwrap(), "alpha\nbeta");
        assert_eq!(m.status, "copied 2 log lines");
        assert!(!m.log_selecting && m.log_sel_active, "copied selection lost its highlight");
    }

    #[test]
    fn plain_log_click_does_not_copy() {
        let (clipboard, copied) = stub_clipboard();
        let mut m = log_model();
        m.clipboard = clipboard;
        m.logs = ["2024-01-01T00:00:00Z alpha".to_string()].into();
        m.update(left(MouseAction::Press, 60, 15));
        m.update(left(MouseAction::Release, 60, 15));
        assert!(copied.lock().unwrap().is_empty() && !m.log_sel_active && m.focus == Focus::Logs);
    }

    #[test]
    fn mouse_wheel_scrolls_logs() {
        let mut m = model();
        (m.width, m.height) = (120, 24);
        m.logs = ["one", "two", "three", "four"].map(String::from).into();
        m.update(wheel(MouseButton::WheelUp, 80));
        assert_eq!((m.log_scroll, m.focus), (3, Focus::Logs));
        m.update(wheel(MouseButton::WheelDown, 80));
        assert_eq!(m.log_scroll, 0);
    }

    #[test]
    fn delete_confirmation_stays_with_original_container() {
        let backend = FakeBackend::new();
        let mut m = model_with(&backend);
        m.width = 80;
        m.profiles = vec![profile("default", "Running")];
        m.containers = vec![container("one", "one", "exited"), container("two", "two", "exited")];
        m.key(crate::tea::Key::runes("d"));
        m.update(wheel(MouseButton::WheelDown, 2));
        let cmd = m.key(crate::tea::Key::runes("y")).expect("delete confirmation did not dispatch");
        crate::testutil::run_all(cmd);
        assert_eq!(backend.state().action_args.last().map(String::as_str), Some("one"));
    }
}
