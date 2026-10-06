//! Message handling: key bindings, refresh reconciliation and async results.

use std::sync::Arc;
use std::time::Duration;

use crate::autostop::{AUTO_STOP_ENV, format_countdown};
use crate::backend::is_running;
use crate::error::Error;
use crate::gocompat;
use crate::menubar_proc::{self, MENUBAR_SUPPORTED};
use crate::model::{ActionMsg, ActiveAction, Cmd, Focus, Model, Msg, Profile, RefreshMsg};
use crate::release::check_for_update_cmd;
use crate::stats::stats_tick;
use crate::tea::{Key, KeyCode, batch};
use crate::view::SPINNER_FRAMES;

const SPINNER_INTERVAL: Duration = Duration::from_millis(120);
const LOG_RETRY_INTERVAL: Duration = Duration::from_secs(1);

pub struct ActionMenuItem {
    pub label: String,
    pub shortcut: &'static str,
    pub enabled: bool,
}

impl Model {
    pub fn init(&mut self) -> Option<Cmd> {
        let refresh = self.refresh_cmd(self.refresh_id, "");
        batch([Some(refresh), self.next_tick(), Some(check_for_update_cmd()), Some(stats_tick())])
    }

    pub fn update(&mut self, msg: Msg) -> Option<Cmd> {
        match msg {
            Msg::Cleanup(msg) => {
                self.cleanup_running = false;
                if msg.profile != self.current_profile_name() {
                    return None;
                }
                self.confirm_cleanup = false;
                if let Some(err) = msg.err {
                    self.err = Some(err);
                    self.status = "cleanup failed".to_string();
                    return None;
                }
                self.storage = None;
                self.storage_requested = None;
                self.status = "cleanup complete".to_string();
                let name = self.current_profile_name();
                batch([Some(self.queue_refresh(&name)), self.poll_storage()])
            }
            Msg::StatsTick => batch([self.poll_stats(), self.poll_storage(), Some(stats_tick())]),
            Msg::Storage(sample) => {
                self.storage_busy = false;
                if sample.profile == self.current_profile_name() {
                    self.storage = Some(sample);
                }
                None
            }
            Msg::Stats(sample) => {
                self.stats_busy = false;
                if sample.profile == self.current_profile_name() {
                    self.overall = Some(sample);
                }
                None
            }
            Msg::Key(key) => self.key(key),
            Msg::Mouse(mouse) => self.mouse(mouse),
            Msg::WindowSize { width, height } => {
                self.width = width;
                self.height = height;
                None
            }
            Msg::Refresh(msg) => self.apply_refresh(msg),
            Msg::Action(msg) => self.apply_action(msg),
            Msg::Tick => {
                let name = self.current_profile_name();
                batch([Some(self.queue_refresh(&name)), self.next_tick()])
            }
            Msg::SpinnerTick => {
                if !self.has_active_actions() {
                    return None;
                }
                self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
                Some(spinner_tick())
            }
            Msg::UpdateCheck(version) => {
                self.update_version = version;
                None
            }
            Msg::Logs(msg) => self.apply_logs(msg),
            Msg::ExecDone(result) => {
                self.err = None;
                self.status = "ready".to_string();
                if let Some(err) = shell_failure(&result) {
                    self.err = Some(err);
                    self.status = "shell failed".to_string();
                }
                let name = self.current_profile_name();
                Some(self.queue_refresh(&name))
            }
            Msg::LogRetry => {
                if !self.follow || self.reader.is_some() {
                    return None;
                }
                let state = self.selected_container()?.state.to_lowercase();
                if !is_running(&state) && state != "restarting" {
                    return Some(log_retry_tick());
                }
                self.resume_follow_logs()
            }
        }
    }

    fn apply_refresh(&mut self, msg: RefreshMsg) -> Option<Cmd> {
        if msg.request_id != 0 {
            if msg.request_id < self.applied_refresh_id {
                return None;
            }
            if !self.profiles.is_empty()
                && !msg.profile_name.is_empty()
                && msg.profile_name != self.current_profile_name()
            {
                return None;
            }
            self.applied_refresh_id = msg.request_id;
        }
        if msg.list_failed {
            self.err = msg.err;
            if !self.has_active_actions() {
                self.status = "connection error".to_string();
            }
            return self.track_idle();
        }
        let old_id = self.selected_id();
        let old_group = self.selected_group_name();
        let active_profile = self.current_profile_name();
        let (previous_err, previous_status) = (self.err.clone(), self.status.clone());
        self.profiles = msg.profiles;
        self.containers = msg.containers;
        self.err = msg.err;
        self.sync_expanded();
        if let Some(index) = find_profile(&self.profiles, &active_profile) {
            self.profile_index = index;
        } else if let Some(index) = find_profile(&self.profiles, &msg.profile_name) {
            self.profile_index = index;
        } else if self.profile_index >= self.profiles.len() {
            self.profile_index = self.profiles.len().saturating_sub(1);
        }
        if self.containers.is_empty() {
            self.container_index = 0;
        } else if let Some(index) = self.find_container_item(&old_id) {
            self.container_index = index;
        } else if let Some(index) = self.find_group_item(&old_group) {
            self.container_index = index;
        } else if old_id.is_empty() && old_group.is_empty() {
            self.container_index = self.first_container_item();
        } else {
            let items = self.list_items().len();
            if self.container_index >= items {
                self.container_index = items.saturating_sub(1);
            }
        }
        if self.has_active_actions() {
            // Keep the in-progress action visible while background refreshes run.
        } else if self.err.is_some() {
            self.status = "connection error".to_string();
        } else if old_id == self.selected_id() && previous_status == "logs failed" && previous_err.is_some() {
            self.err = previous_err;
            self.status = previous_status;
        } else {
            self.status = "ready".to_string();
        }
        self.validate_delete_confirmation();
        let auto_stop = self.track_idle();
        if old_id != self.selected_id() {
            self.stop_logs();
            self.reset_logs();
            let id = self.selected_id();
            if !id.is_empty() {
                let req = crate::backend::LogRequest { follow: self.follow, ..Default::default() };
                match self.backend.open_logs(&self.current_profile_name(), &id, &req) {
                    Ok(reader) => {
                        self.reader = Some(reader);
                        return batch([self.read_logs_cmd(), auto_stop]);
                    }
                    Err(err) => {
                        self.err = Some(err.to_string());
                        self.status = "logs failed".to_string();
                        return auto_stop;
                    }
                }
            }
        }
        auto_stop
    }

    fn apply_action(&mut self, msg: ActionMsg) -> Option<Cmd> {
        let action = self.active_actions.remove(&msg.request_id)?;
        match msg.err {
            Some(err) => {
                self.err = Some(err);
                self.status = format!("{} failed", action.label);
            }
            None => {
                self.err = None;
                self.status = format!("{} complete", action.label);
            }
        }
        let name = self.current_profile_name();
        Some(self.queue_refresh(&name))
    }

    pub fn key(&mut self, msg: Key) -> Option<Cmd> {
        let key = msg.name();
        if key == "ctrl+c" {
            self.stop_logs();
            return Some(Cmd::Quit);
        }
        if self.confirm_cleanup {
            match key.as_str() {
                "y" => {
                    self.confirm_cleanup = false;
                    return self.cleanup_cmd();
                }
                "enter" => {
                    self.confirm_cleanup = false;
                    if self.cleanup_choice == 1 {
                        return self.cleanup_cmd();
                    }
                }
                "up" | "k" => self.cleanup_choice = 0,
                "down" | "j" => self.cleanup_choice = 1,
                "n" | "esc" | "q" | "c" => self.confirm_cleanup = false,
                _ => {}
            }
            return None;
        }
        if self.usage_overview {
            if key == "c" && !self.cleanup_running {
                self.confirm_cleanup = true;
                self.cleanup_choice = 0;
                return None;
            }
            if matches!(key.as_str(), "esc" | "q" | "u" | "?") {
                self.usage_overview = false;
            }
            return None;
        }
        if self.log_search_editing {
            return self.log_search_key(&msg);
        }
        if self.search_editing {
            return self.search_key(&msg);
        }
        if self.action_menu {
            return self.action_menu_key(msg);
        }
        if self.confirm_delete {
            match key.as_str() {
                "y" => return self.dispatch_delete(),
                "enter" => {
                    if self.delete_choice == 1 {
                        return self.dispatch_delete();
                    }
                    self.cancel_delete_confirmation("ready");
                }
                "up" | "k" => self.delete_choice = 0,
                "down" | "j" => self.delete_choice = 1,
                "n" | "esc" | "q" => self.cancel_delete_confirmation("ready"),
                _ => {}
            }
            return None;
        }

        match key.as_str() {
            "c" => {
                self.usage_overview = true;
                self.confirm_cleanup = true;
                self.cleanup_choice = 0;
                return batch([self.poll_stats(), self.poll_storage()]);
            }
            "u" => {
                self.usage_overview = true;
                return batch([self.poll_stats(), self.poll_storage()]);
            }
            "L" => {
                self.log_search_editing = true;
                self.log_search_before = self.log_query.clone();
                self.focus = Focus::Logs;
                self.pause_logs();
            }
            "T" => {
                self.log_timestamps = !self.log_timestamps;
                self.status = format!("log timestamps {}", on_off(self.log_timestamps));
                let enabled = self.log_timestamps;
                self.persist_setting(|s| s.log_timestamps = enabled);
            }
            "w" => {
                self.log_wrap = !self.log_wrap;
                self.status = format!("log wrap {}", on_off(self.log_wrap));
                let enabled = self.log_wrap;
                self.persist_setting(|s| s.log_wrap = enabled);
            }
            "/" => {
                self.search_editing = true;
                self.search_before = self.search_query.clone();
                self.focus = Focus::Containers;
            }
            "R" => {
                let old_id = self.selected_id();
                self.running_only = !self.running_only;
                return self.filter_selection(&old_id);
            }
            "esc" => {
                let old_id = self.selected_id();
                self.search_query.clear();
                self.running_only = false;
                return self.filter_selection(&old_id);
            }
            "?" => {
                self.action_menu = true;
                self.action_index = 0;
            }
            "q" => {
                self.stop_logs();
                return Some(Cmd::Quit);
            }
            "tab" | "left" | "right" => self.focus = self.focus.toggled(),
            "[" | "]" => {
                let count = self.profiles.len();
                if count > 1 {
                    self.profile_index = if key == "[" {
                        (self.profile_index + count - 1) % count
                    } else {
                        (self.profile_index + 1) % count
                    };
                    self.cancel_delete_confirmation("ready");
                    self.stop_logs();
                    self.containers.clear();
                    self.reset_logs();
                    self.follow = false;
                    let name = self.current_profile_name();
                    self.status = format!("switching to {name}");
                    return Some(self.queue_refresh(&name));
                }
            }
            "up" | "k" => {
                if self.focus == Focus::Containers && self.container_index > 0 {
                    self.container_index -= 1;
                    return self.reload_selected_logs(false);
                }
            }
            "down" | "j" => {
                if self.focus == Focus::Containers && self.container_index + 1 < self.list_items().len() {
                    self.container_index += 1;
                    return self.reload_selected_logs(false);
                }
            }
            "r" => {
                self.status = "refreshing".to_string();
                let name = self.current_profile_name();
                return Some(self.queue_refresh(&name));
            }
            "a" => {
                if self.auto_stop_pinned {
                    self.status = format!("idle auto-stop is set by {AUTO_STOP_ENV}");
                    return None;
                }
                self.auto_stop = !self.auto_stop;
                let saved = if self.auto_stop {
                    self.status = format!("idle auto-stop on ({})", format_countdown(self.auto_stop_after));
                    gocompat::format_duration(self.auto_stop_after)
                } else {
                    self.status = "idle auto-stop off".to_string();
                    self.clear_idle();
                    "off".to_string()
                };
                self.persist_setting(|s| s.auto_stop = saved);
            }
            "m" => {
                if !MENUBAR_SUPPORTED {
                    return None;
                }
                let toggled = if self.menubar { menubar_proc::stop_menubar() } else { menubar_proc::spawn_menubar() };
                if let Err(err) = toggled {
                    self.err = Some(err.to_string());
                    self.status = "menu bar toggle failed".to_string();
                    return None;
                }
                self.menubar = !self.menubar;
                self.status = format!("menu bar item {}", on_off(self.menubar));
                let enabled = self.menubar;
                self.persist_setting(|s| s.menubar = Some(enabled));
            }
            "s" => {
                let stopped = self.current_profile().is_none_or(|p| !is_running(&p.status));
                if !self.has_active_profile_action() && stopped {
                    let name = self.current_profile_name();
                    self.status = format!("starting {name}");
                    let command = self.action_cmd(&name, "start", "colima", &["start", "--profile", &name]);
                    return batch([Some(command), Some(spinner_tick())]);
                }
            }
            "x" => {
                if let Some(name) = self.current_profile().filter(|p| is_running(&p.status)).map(|p| p.name.clone())
                    && !self.has_active_profile_action()
                {
                    self.status = format!("stopping {name}");
                    let command = self.action_cmd(&name, "stop", "colima", &["stop", "--profile", &name]);
                    return batch([Some(command), Some(spinner_tick())]);
                }
            }
            "e" => {
                if let Some(c) = self.selected_container().cloned() {
                    if let Some(action) = self.active_container_action(&c.id) {
                        self.status = format!("{} {}", action_progress_label(&action.label), c.name);
                        return None;
                    }
                    if !is_running(&c.state) {
                        self.status = "start the container before opening a shell".to_string();
                        return None;
                    }
                    self.status = format!("shell: {}", c.list_name());
                    let shell = self.backend.shell(&self.current_profile_name(), &c.id);
                    let program = shell.get_program().to_string_lossy().into_owned();
                    return Some(Cmd::exec(shell, move |status| {
                        Msg::ExecDone(status.map_err(|err| Error::spawn(&program, err)))
                    }));
                }
            }
            "t" => {
                if let Some(c) = self.selected_container().cloned() {
                    if let Some(action) = self.active_container_action(&c.id) {
                        self.status = format!("{} {}", action_progress_label(&action.label), c.name);
                        return None;
                    }
                    self.status = format!("restarting {}", c.name);
                    let profile = self.current_profile_name();
                    let command = self.action_cmd(&profile, "restart", "docker", &["restart", &c.id]);
                    return batch([Some(command), Some(spinner_tick())]);
                }
            }
            "enter" => {
                if self.selected_item().is_some_and(|item| item.group_header) {
                    self.toggle_selected_group();
                } else if let Some(c) = self.selected_container().cloned() {
                    if let Some(action) = self.active_container_action(&c.id) {
                        self.status = format!("{} {}", action_progress_label(&action.label), c.name);
                        return None;
                    }
                    let verb = if c.state == "running" { "stop" } else { "start" };
                    self.status = format!("{} {}", if verb == "start" { "starting" } else { "stopping" }, c.name);
                    let profile = self.current_profile_name();
                    let command = self.action_cmd(&profile, verb, "docker", &[verb, &c.id]);
                    return batch([Some(command), Some(spinner_tick())]);
                }
            }
            "d" => {
                if let Some(c) = self.selected_container().cloned() {
                    if let Some(action) = self.active_container_action(&c.id) {
                        self.status = format!("{} {}", action_progress_label(&action.label), c.name);
                        return None;
                    }
                    if c.state == "running" {
                        self.status = "stop the container before deleting it".to_string();
                    } else {
                        self.confirm_delete = true;
                        self.delete_profile = self.current_profile_name();
                        self.delete_id = c.id.clone();
                        self.delete_choice = 0;
                        self.status = format!("delete {}? y/n", c.name);
                    }
                }
            }
            "y" => return self.copy_selected_details(),
            "Y" => return self.copy_filtered_logs(),
            "l" => return self.reload_selected_logs(false),
            "f" => {
                if self.follow {
                    self.pause_logs();
                    return None;
                }
                self.follow = true;
                return self.reload_selected_logs(false);
            }
            "home" => {
                self.follow = false;
                self.status = "loading all logs".to_string();
                return self.reload_selected_logs(true);
            }
            "pgup" | "pgdown" | "end" => {
                self.pause_logs();
                self.scroll_logs(&key);
            }
            _ => {}
        }
        None
    }

    fn dispatch_delete(&mut self) -> Option<Cmd> {
        if let Some(c) = self.delete_target().filter(|c| c.state != "running").cloned() {
            self.confirm_delete = false;
            self.delete_profile.clear();
            self.delete_id.clear();
            self.delete_choice = 0;
            self.status = format!("deleting {}", c.name);
            let profile = self.current_profile_name();
            return Some(self.action_cmd(&profile, "delete", "docker", &["rm", &c.id]));
        }
        self.cancel_delete_confirmation("delete canceled: container changed");
        None
    }

    pub fn action_menu_items(&self) -> Vec<ActionMenuItem> {
        let item = |label: String, shortcut, enabled| ActionMenuItem { label, shortcut, enabled };
        let (profile_label, profile_shortcut) = match self.current_profile() {
            Some(p) if is_running(&p.status) => ("stop colima", "x"),
            _ => ("start colima", "s"),
        };
        let container = self.selected_container();
        let has = container.is_some();
        let busy = container.is_some_and(|c| self.active_container_action(&c.id).is_some());
        let name = container.map_or("selected container", |c| c.list_name());
        let running = container.is_some_and(|c| c.state == "running");
        let container_label = match container {
            None => "start/stop selected container".to_string(),
            Some(_) if running => format!("stop {name}"),
            Some(_) => format!("start {name}"),
        };
        let follow_label =
            if self.follow { format!("pause logs for {name}") } else { format!("follow logs for {name}") };
        let window = format_countdown(self.auto_stop_after);
        let auto_stop_label = if self.auto_stop {
            format!("disable idle auto-stop ({window})")
        } else {
            format!("enable idle auto-stop ({window})")
        };
        let mut items = vec![
            item(profile_label.to_string(), profile_shortcut, !self.has_active_profile_action()),
            item(auto_stop_label, "a", true),
            item(container_label, "enter", has && !busy),
            item(format!("restart {name}"), "t", has && !busy),
            item(format!("open shell in {name}"), "e", has && !busy && running),
            item(format!("delete {name}"), "d", has && !busy && !running),
            item(format!("reload logs for {name}"), "l", has),
            item(follow_label, "f", has),
            item(format!("load all logs for {name}"), "home", has),
            item(format!("copy details for {name}"), "y", has),
            item("copy logs to clipboard".to_string(), "Y", has && !self.logs.is_empty()),
            item("search log text".to_string(), "L", has),
            item("toggle log timestamps".to_string(), "T", true),
            item((if self.log_wrap { "trim long log lines" } else { "wrap long log lines" }).to_string(), "w", true),
            item("clean up reclaimable docker storage".to_string(), "c", !self.cleanup_running),
            item("docker usage overview".to_string(), "u", true),
            item("refresh".to_string(), "r", true),
            item("search containers".to_string(), "/", true),
            item((if self.running_only { "show all states" } else { "show running only" }).to_string(), "R", true),
        ];
        if MENUBAR_SUPPORTED {
            let label = if self.menubar { "disable macOS menu bar item" } else { "enable macOS menu bar item" };
            items.insert(2, item(label.to_string(), "m", true));
        }
        items
    }

    fn action_menu_key(&mut self, msg: Key) -> Option<Cmd> {
        let items = self.action_menu_items();
        if items.is_empty() {
            self.action_menu = false;
            return None;
        }
        if self.action_index >= items.len() {
            self.action_index = items.len() - 1;
        }
        match msg.name().as_str() {
            "esc" | "?" | "q" => {
                self.action_menu = false;
                None
            }
            "up" | "k" => {
                self.action_index = (self.action_index + items.len() - 1) % items.len();
                None
            }
            "down" | "j" => {
                self.action_index = (self.action_index + 1) % items.len();
                None
            }
            "enter" => {
                let item = &items[self.action_index];
                if !item.enabled {
                    return None;
                }
                self.action_menu = false;
                self.key(shortcut_key(item.shortcut))
            }
            _ => {
                self.action_menu = false;
                self.key(msg)
            }
        }
    }

    pub fn validate_delete_confirmation(&mut self) {
        if !self.confirm_delete || self.delete_target().is_some() {
            return;
        }
        self.cancel_delete_confirmation("delete canceled: container changed");
    }

    pub fn delete_target(&self) -> Option<&crate::model::Container> {
        if !self.confirm_delete || self.delete_profile != self.current_profile_name() {
            return None;
        }
        self.containers.iter().find(|c| c.id == self.delete_id)
    }

    fn cancel_delete_confirmation(&mut self, status: &str) {
        self.confirm_delete = false;
        self.delete_profile.clear();
        self.delete_id.clear();
        self.delete_choice = 0;
        self.status = status.to_string();
    }

    pub fn queue_refresh(&mut self, profile: &str) -> Cmd {
        self.refresh_id += 1;
        self.refresh_cmd(self.refresh_id, profile)
    }

    pub fn refresh_cmd(&self, request_id: u64, profile: &str) -> Cmd {
        let backend = Arc::clone(&self.backend);
        let profile = profile.to_string();
        Cmd::run(move || {
            let profiles = match backend.profiles() {
                Ok(profiles) => profiles,
                Err(err) => {
                    let name = if profile.is_empty() { "default".to_string() } else { profile };
                    return Msg::Refresh(RefreshMsg {
                        profile_name: name,
                        request_id,
                        profiles: Vec::new(),
                        containers: Vec::new(),
                        err: Some(err.to_string()),
                        list_failed: true,
                    });
                }
            };
            let name = match (profile.is_empty(), profiles.first()) {
                (false, _) => profile,
                (true, Some(first)) => first.name.clone(),
                (true, None) => "default".to_string(),
            };
            let (containers, mut err) = match backend.containers(&name) {
                Ok(containers) => (containers, None),
                Err(err) => (Vec::new(), Some(err.to_string())),
            };
            if profiles.iter().any(|p| p.name == name && !is_running(&p.status)) {
                err = None;
            }
            Msg::Refresh(RefreshMsg { profile_name: name, request_id, profiles, containers, err, list_failed: false })
        })
    }

    pub fn has_active_actions(&self) -> bool {
        !self.active_actions.is_empty()
    }

    pub fn active_container_action(&self, id: &str) -> Option<&ActiveAction> {
        self.active_actions.values().find(|action| action.container_id == id)
    }

    pub fn has_active_profile_action(&self) -> bool {
        self.active_actions.values().any(|action| action.container_id.is_empty())
    }

    pub fn action_cmd(&mut self, profile: &str, label: &str, command: &str, args: &[&str]) -> Cmd {
        self.next_action_id += 1;
        let request_id = self.next_action_id;
        let container_id = match args {
            [_, .., last] if command == "docker" => last.to_string(),
            _ => String::new(),
        };
        self.active_actions.insert(request_id, ActiveAction { container_id, label: label.to_string() });
        let backend = Arc::clone(&self.backend);
        let (profile, command) = (profile.to_string(), command.to_string());
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        Cmd::run(move || {
            let err = backend.action(&profile, &command, &args).err().map(|err| err.to_string());
            Msg::Action(ActionMsg { request_id, err })
        })
    }

    /// Preserves the selected identity when possible; never reuses a filtered row index.
    pub fn filter_selection(&mut self, old_id: &str) -> Option<Cmd> {
        self.container_index = self.find_container_item(old_id).unwrap_or_else(|| self.first_container_item());
        if old_id != self.selected_id() {
            return self.reload_selected_logs(false);
        }
        None
    }

    fn search_key(&mut self, msg: &Key) -> Option<Cmd> {
        let old_id = self.selected_id();
        match &msg.code {
            KeyCode::Enter => self.search_editing = false,
            KeyCode::Esc => {
                self.search_query = self.search_before.clone();
                self.search_editing = false;
            }
            KeyCode::Backspace | KeyCode::Delete => {
                self.search_query.pop();
            }
            KeyCode::Ctrl('u') => self.search_query.clear(),
            KeyCode::Space => self.search_query.push(' '),
            KeyCode::Runes(text) if self.search_query.chars().count() + text.chars().count() <= 256 => {
                self.search_query.push_str(text);
            }
            _ => {}
        }
        self.filter_selection(&old_id)
    }
}

pub fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

pub fn shortcut_key(shortcut: &str) -> Key {
    match shortcut {
        "enter" => Key::new(KeyCode::Enter),
        "home" => Key::new(KeyCode::Home),
        _ => Key::runes(shortcut),
    }
}

pub fn find_profile(profiles: &[Profile], name: &str) -> Option<usize> {
    profiles.iter().position(|p| p.name == name)
}

pub fn spinner_tick() -> Cmd {
    Cmd::Tick(SPINNER_INTERVAL, Msg::SpinnerTick)
}

pub fn log_retry_tick() -> Cmd {
    Cmd::Tick(LOG_RETRY_INTERVAL, Msg::LogRetry)
}

/// Separates real exec failures from the user's own shell exit status:
/// 126/127 mean no shell could be started inside the container.
pub fn shell_failure(result: &Result<std::process::ExitStatus, Error>) -> Option<String> {
    match result {
        Ok(status) if status.success() => None,
        Ok(status) => matches!(status.code(), Some(126 | 127)).then(|| gocompat::exit_status_text(*status)),
        Err(err) => Some(err.to_string()),
    }
}

pub fn action_progress_label(action: &str) -> String {
    match action {
        "stop" => "stopping…".to_string(),
        "start" => "starting…".to_string(),
        "restart" => "restarting…".to_string(),
        "delete" => "deleting…".to_string(),
        _ => format!("{action}…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use crate::model::Container;
    use crate::testutil::{FakeBackend, container, model, model_with, profile, refresh, run_all, run_one};

    fn key(text: &str) -> Key {
        Key::runes(text)
    }

    fn refresh_msg(profile_name: &str, request_id: u64, profiles: Vec<Profile>, containers: Vec<Container>) -> Msg {
        Msg::Refresh(RefreshMsg {
            profile_name: profile_name.into(),
            request_id,
            profiles,
            containers,
            err: None,
            list_failed: false,
        })
    }

    #[test]
    fn model_injects_backend_timer_and_logs() {
        let backend = FakeBackend::new();
        backend.state().profiles = vec![profile("dev", "Running")];
        backend.state().containers = vec![container("id", "test", "running")];
        let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let mut m = model_with(&backend);
        m.tick = Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(Cmd::Tick(Duration::ZERO, Msg::Tick))
        });
        let Msg::Refresh(msg) = run_one(m.refresh_cmd(1, "dev")) else { panic!("expected refresh") };
        assert_eq!((msg.profile_name.as_str(), msg.containers.len()), ("dev", 1));
        assert_eq!(backend.state().containers_profile, "dev");
        assert!(m.next_tick().is_some() && ticks.load(std::sync::atomic::Ordering::SeqCst) == 1);
        let Msg::Action(action) = run_one(m.action_cmd("dev", "restart", "docker", &["restart", "id"])) else {
            panic!("expected action")
        };
        assert!(action.err.is_none());
        let state = backend.state();
        assert_eq!(
            (state.action_profile.as_str(), state.action_command.as_str(), state.action_args.join(" ")),
            ("dev", "docker", "restart id".to_string())
        );
        drop(state);
        m.profiles = vec![profile("dev", "Running")];
        m.containers = vec![container("id", "test", "running")];
        m.reload_selected_logs(false);
        let state = backend.state();
        assert_eq!((state.log_profile.as_str(), state.log_id.as_str()), ("dev", "id"));
    }

    #[test]
    fn delete_confirmation_dispatches_one_action() {
        let backend = FakeBackend::new();
        let mut m = model_with(&backend);
        m.profiles = vec![profile("default", "Running")];
        m.containers = vec![container("id", "test", "exited")];
        m.confirm_delete = true;
        (m.delete_profile, m.delete_id) = ("default".into(), "id".into());
        let cmd = m.key(key("y")).expect("delete did not dispatch");
        assert!(!m.confirm_delete && m.active_actions.len() == 1);
        let request_id = *m.active_actions.keys().next().unwrap();
        assert!(
            m.key(key("y")).is_none() && m.active_actions.len() == 1,
            "duplicate confirmation started another action"
        );
        let Msg::Action(msg) = run_one(cmd) else { panic!("expected action") };
        assert_eq!((msg.request_id, backend.state().action_calls), (request_id, 1));
    }

    #[test]
    fn delete_confirmation_modal_selects_delete() {
        let mut m = model();
        (m.width, m.height) = (100, 24);
        m.profiles = vec![profile("default", "Running")];
        m.containers = vec![container("one", "one", "exited")];
        m.update(Msg::Key(key("d")));
        m.update(Msg::Key(Key::new(KeyCode::Down)));
        assert!(m.delete_choice == 1 && m.view().contains("delete container?"));
        assert!(m.update(Msg::Key(Key::new(KeyCode::Enter))).is_some() && !m.confirm_delete);
    }

    #[test]
    fn ctrl_c_quits_from_modals() {
        for setup in [|m: &mut Model| m.action_menu = true, |m: &mut Model| m.confirm_delete = true] {
            let mut m = model();
            setup(&mut m);
            assert!(matches!(m.key(Key::new(KeyCode::Ctrl('c'))), Some(Cmd::Quit)));
        }
    }

    #[test]
    fn refresh_accepts_completed_request_when_newer_one_is_queued() {
        let mut m = model();
        m.refresh_id = 10;
        m.profiles = vec![profile("default", "Running")];
        m.update(Msg::Tick);
        let profiles = m.profiles.clone();
        m.update(refresh_msg("default", 10, profiles, vec![container("new", "new", "")]));
        assert_eq!(m.containers[0].id, "new", "completed refresh was discarded");
    }

    #[test]
    fn action_ignores_stale_completion() {
        let mut m = model();
        m.active_actions.insert(2, ActiveAction { container_id: String::new(), label: "start".into() });
        m.status = "starting default".into();
        assert!(m.update(Msg::Action(ActionMsg { request_id: 1, err: None })).is_none());
        assert!(m.active_actions.len() == 1 && m.status == "starting default");
        assert!(m.update(Msg::Action(ActionMsg { request_id: 2, err: None })).is_some());
        assert!(m.active_actions.is_empty() && m.status == "start complete");
    }

    #[test]
    fn active_action_allows_navigation_and_keeps_status() {
        let mut m = model();
        m.active_actions.insert(1, ActiveAction { container_id: "one".into(), label: "stop".into() });
        m.next_action_id = 1;
        m.status = "stopping api".into();
        m.containers = vec![container("one", "api", "running"), container("two", "worker", "running")];
        m.update(Msg::Key(Key::new(KeyCode::Down)));
        assert!(m.container_index == 1 && m.active_actions.len() == 1);
        let before_enter = m.clone();
        assert!(m.update(Msg::Key(Key::new(KeyCode::Enter))).is_some() && m.active_actions.len() == 2);
        let mut m = before_enter;
        let containers = m.containers.clone();
        m.update(refresh(vec![profile("default", "Running")], containers));
        assert_eq!(m.status, "stopping api", "refresh replaced active action status");
    }

    #[test]
    fn start_key_works_without_profile_record() {
        let mut m = model();
        m.status = "ready".into();
        assert!(m.key(key("s")).is_some());
        assert_eq!(m.status, "starting default");
    }

    #[test]
    fn action_menu_runs_selected_action() {
        let mut m = model();
        m.profiles = vec![profile("default", "Stopped")];
        m.action_menu = true;
        let cmd = m.update(Msg::Key(Key::new(KeyCode::Enter)));
        assert!(!m.action_menu && m.status == "starting default" && cmd.is_some());
    }

    #[test]
    fn action_menu_forwards_shortcut_keys() {
        let mut m = model();
        m.profiles = vec![profile("default", "Running")];
        m.containers = vec![container("one", "one", "running")];
        m.action_menu = true;
        let cmd = m.update(Msg::Key(key("t")));
        assert!(!m.action_menu && m.status == "restarting one" && cmd.is_some());
        m.action_menu = true;
        m.update(Msg::Key(key("z")));
        assert!(!m.action_menu, "unbound key left the menu open");
    }

    #[test]
    fn w_key_toggles_log_wrapping() {
        let mut m = model();
        assert!(m.key(shortcut_key("w")).is_none() && m.log_wrap);
        m.key(shortcut_key("w"));
        assert!(!m.log_wrap);
    }

    #[test]
    fn tab_changes_focus() {
        let mut m = model();
        m.update(Msg::Key(Key::new(KeyCode::Tab)));
        assert_eq!(m.focus, Focus::Logs);
    }

    #[test]
    fn refresh_preserves_log_error() {
        let mut m = model();
        m.profiles = vec![profile("default", "Running")];
        m.containers = vec![container("id", "test", "running")];
        (m.err, m.status) = (Some("docker error".into()), "logs failed".into());
        let (profiles, containers) = (m.profiles.clone(), m.containers.clone());
        m.update(refresh(profiles, containers));
        assert!(m.err.is_some() && m.status == "logs failed");
    }

    #[test]
    fn refresh_drops_stale_response() {
        let mut m = model();
        m.profiles = vec![profile("default", "Running"), profile("dev", "Running")];
        m.profile_index = 1;
        m.containers = vec![container("old", "old", "running")];
        m.refresh_id = 2;
        let cmd = m.update(refresh_msg(
            "default",
            1,
            vec![profile("default", "Running")],
            vec![container("stale", "stale", "running")],
        ));
        assert!(cmd.is_none() && m.current_profile_name() == "dev" && m.containers[0].id == "old");
    }

    #[test]
    fn failed_profile_listing_keeps_current_profile() {
        let backend = FakeBackend::new();
        let profiles = vec![profile("default", "Running"), profile("work", "Running")];
        let containers = vec![container("id", "api", "running")];
        {
            let mut state = backend.state();
            (state.profiles, state.containers) = (profiles.clone(), containers.clone());
            state.profiles_err = Some("colima busy".into());
        }
        let mut m = model_with(&backend);
        (m.profiles, m.profile_index, m.containers) = (profiles, 1, containers);
        let name = m.current_profile_name();
        let msg = run_one(m.queue_refresh(&name));
        m.update(msg);
        assert!(
            m.current_profile_name() == "work"
                && m.containers.len() == 1
                && m.err.is_some()
                && m.status == "connection error"
        );
        backend.state().profiles_err = None;
        let name = m.current_profile_name();
        let msg = run_one(m.queue_refresh(&name));
        m.update(msg);
        assert!(m.current_profile_name() == "work" && backend.state().containers_profile == "work" && m.err.is_none());
    }

    #[test]
    fn refresh_preserves_profile_name() {
        let mut m = model();
        m.profiles = vec![profile("default", "Running"), profile("dev", "Running")];
        (m.profile_index, m.refresh_id) = (1, 1);
        m.update(refresh_msg("dev", 1, vec![profile("dev", "Running"), profile("default", "Running")], Vec::new()));
        assert!(m.current_profile_name() == "dev" && m.profile_index == 0);
    }

    #[test]
    fn shell_key_opens_exec_for_running_container() {
        let backend = FakeBackend::new();
        let mut m = model_with(&backend);
        m.profiles = vec![profile("dev", "Running")];
        m.containers = vec![container("one", "web", "running")];
        assert!(matches!(m.key(key("e")), Some(Cmd::Exec(..))));
        let state = backend.state();
        assert_eq!(
            (m.status.as_str(), state.shell_profile.as_str(), state.shell_id.as_str()),
            ("shell: web", "dev", "one")
        );
    }

    #[test]
    fn shell_key_refuses_stopped_container() {
        let backend = FakeBackend::new();
        let mut m = model_with(&backend);
        m.profiles = vec![profile("dev", "Running")];
        m.containers = vec![container("one", "web", "exited")];
        assert!(m.key(key("e")).is_none() && backend.state().shell_id.is_empty());
        assert_eq!(m.status, "start the container before opening a shell");
    }

    #[test]
    fn shell_failure_separates_user_exit_from_exec_failure() {
        let exit = |code: i32| Ok(ExitStatus::from_raw(code << 8));
        assert_eq!(shell_failure(&exit(0)), None);
        assert_eq!(shell_failure(&exit(1)), None, "user shell exit reported as failure");
        assert_eq!(shell_failure(&exit(127)).as_deref(), Some("exit status 127"));
        assert!(shell_failure(&Err(Error::invalid("no terminal"))).is_some());
    }

    #[test]
    fn exec_done_refreshes_and_reports_failure() {
        let mut m = model();
        m.profiles = vec![profile("default", "Running")];
        assert!(m.update(Msg::ExecDone(Ok(ExitStatus::from_raw(0)))).is_some());
        assert!(m.status == "ready" && m.err.is_none());
        m.update(Msg::ExecDone(Err(Error::invalid("no terminal"))));
        assert!(m.status == "shell failed" && m.err.is_some());
    }

    #[test]
    fn profile_switch_clears_view_and_refreshes() {
        let mut m = model();
        m.profiles = vec![profile("default", "Running"), profile("work", "Running")];
        m.containers = vec![container("id", "api", "running")];
        m.logs = ["line".to_string()].into();
        m.follow = true;
        let cmd = m.key(key("]")).expect("profile switch did not refresh");
        assert!(m.containers.is_empty() && m.logs.is_empty() && !m.follow && m.status == "switching to work");
        assert!(matches!(run_all(cmd).as_slice(), [Msg::Refresh(r)] if r.profile_name == "work"));
    }
}
