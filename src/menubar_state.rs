//! Platform-neutral menu bar logic: polling, idle auto-stop for every
//! profile, and the menu as data. The tray module only renders it.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::autostop::{AUTO_STOP_ENV, format_countdown, is_active, resolve_auto_stop};
use crate::backend::{Backend, is_running};
use crate::error::Error;
use crate::gocompat;
use crate::model::Profile;
use crate::settings::{load_settings, update_settings};
use crate::view::{human_bytes, truncate};

pub const MENUBAR_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long each running profile has had no active containers.
#[derive(Default)]
pub struct IdleTracker {
    idle_since: HashMap<String, Instant>,
}

impl IdleTracker {
    /// Updates one profile's idle state and reports whether the idle window
    /// elapsed; a fire clears the entry so the stop dispatches only once.
    pub fn observe(&mut self, name: &str, active: bool, now: Instant, after: Duration) -> bool {
        if active {
            self.idle_since.remove(name);
            return false;
        }
        let Some(&since) = self.idle_since.get(name) else {
            self.idle_since.insert(name.to_string(), now);
            return false;
        };
        if now.saturating_duration_since(since) < after {
            return false;
        }
        self.idle_since.remove(name);
        true
    }

    pub fn clear(&mut self, name: &str) {
        self.idle_since.remove(name);
    }

    pub fn remaining(&self, name: &str, now: Instant, after: Duration) -> Option<Duration> {
        let since = self.idle_since.get(name)?;
        Some(after.saturating_sub(now.saturating_duration_since(*since)))
    }
}

/// The menu bar's view of the auto-stop setting, re-resolved every poll so
/// toggles from the TUI apply without a restart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoStopState {
    pub after: Duration,
    pub enabled: bool,
    pub env: bool,
    pub err: Option<String>,
}

impl AutoStopState {
    pub fn resolve(settings: Option<&std::path::Path>, env_value: &str) -> Self {
        let resolved = load_settings(settings).and_then(|saved| {
            let path = settings.map(|p| p.display().to_string()).unwrap_or_default();
            resolve_auto_stop(env_value, &saved, &path)
        });
        match resolved {
            Ok((after, enabled)) => Self { after, enabled, env: !env_value.trim().is_empty(), err: None },
            Err(err) => Self { err: Some(err.to_string()), ..Self::default() },
        }
    }

    /// The auto-stop item text; env-pinned and broken settings render as a
    /// non-clickable state description instead of a toggle.
    pub fn label(&self) -> String {
        match self {
            Self { err: Some(_), .. } => "idle auto-stop: invalid setting".to_string(),
            Self { env: true, enabled: true, .. } => {
                format!("idle auto-stop: {} ({AUTO_STOP_ENV})", format_countdown(self.after))
            }
            Self { env: true, .. } => format!("idle auto-stop: off ({AUTO_STOP_ENV})"),
            Self { enabled: true, .. } => "Disable auto-stop".to_string(),
            _ => "Enable auto-stop".to_string(),
        }
    }
}

pub fn menubar_running(profiles: &[Profile]) -> usize {
    profiles.iter().filter(|p| is_running(&p.status)).count()
}

/// The text next to the menu bar icon: the running-profile count when
/// several run, otherwise nothing (the icon alone shows the state).
pub fn menubar_title(profiles: &[Profile]) -> String {
    match menubar_running(profiles) {
        running if running > 1 => running.to_string(),
        _ => String::new(),
    }
}

/// Scales the icon's alpha down for the muted state shown while nothing
/// runs; color is dropped because template icons only use alpha.
pub fn dimmed_rgba(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>().0.iter().flat_map(|pixel| [0, 0, 0, (u16::from(pixel[3]) * 2 / 5) as u8]).collect()
}

pub fn menubar_profile_line(p: &Profile) -> String {
    let status = if p.status.is_empty() { "Unknown" } else { &p.status };
    format!("{} — {status}", p.name)
}

pub fn menubar_profile_details(p: &Profile) -> String {
    let mut details = format!("{} cpu · {} ram · {} disk", p.cpus, human_bytes(p.memory), human_bytes(p.disk));
    if !p.arch.is_empty() {
        details += &format!(" · {}", p.arch);
    }
    details
}

/// Identifies the menu-relevant profile state; the menu is rebuilt only when
/// the full signature changes, so an open menu is not torn down every poll.
pub fn menubar_signature(profiles: &[Profile]) -> String {
    profiles.iter().map(|p| format!("{}|{}|{}|{}|{};", p.name, p.status, p.cpus, p.memory, p.disk)).collect()
}

/// Escapes a value for use inside an AppleScript string literal.
pub fn applescript_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Profile { name: String, command: &'static str },
    ToggleAutoStop(AutoStopState),
    Open,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuEntry {
    /// A disabled informational row.
    Label(String),
    Item(String, MenuAction),
    Submenu(String, Vec<MenuEntry>),
    Separator,
}

/// What one poll asks the tray to show; `dim` and `menu` are set only when
/// they changed.
pub struct Frame {
    pub title: String,
    pub dim: Option<bool>,
    pub menu: Option<Vec<MenuEntry>>,
}

/// State shared by the poll loop and menu click handlers.
pub struct Shared {
    backend: Arc<dyn Backend>,
    settings: Option<PathBuf>,
    env_auto_stop: String,
    busy: Mutex<BTreeSet<String>>,
    last_err: Mutex<String>,
    kick: SyncSender<()>,
}

impl Shared {
    pub fn new(
        backend: Arc<dyn Backend>,
        settings: Option<PathBuf>,
        env_auto_stop: String,
        kick: SyncSender<()>,
    ) -> Arc<Self> {
        Arc::new(Self {
            backend,
            settings,
            env_auto_stop,
            busy: Mutex::new(BTreeSet::new()),
            last_err: Mutex::new(String::new()),
            kick,
        })
    }

    /// Requests an immediate sync without blocking.
    pub fn kick(&self) {
        let _ = self.kick.try_send(());
    }

    /// Runs a clicked action; Open and Quit belong to the tray.
    pub fn run(self: &Arc<Self>, action: &MenuAction) {
        match action {
            MenuAction::Profile { name, command } => self.profile_action(name, command),
            MenuAction::ToggleAutoStop(auto) => self.toggle_auto_stop(auto),
            MenuAction::Open | MenuAction::Quit => {}
        }
    }

    fn profile_action(self: &Arc<Self>, name: &str, command: &'static str) {
        let shared = Arc::clone(self);
        let name = name.to_string();
        thread::spawn(move || {
            shared.set_busy(&name, true);
            shared.kick();
            let args = [command.to_string(), "--profile".to_string(), name.clone()];
            let result = shared.backend.action(&name, "colima", &args);
            shared.set_busy(&name, false);
            shared.set_last_err(result.err());
            shared.kick();
        });
    }

    /// Persists the flipped setting; the next sync re-reads it, so the TUI and
    /// the menu bar stay on the one saved value.
    fn toggle_auto_stop(&self, auto: &AutoStopState) {
        let saved = if auto.enabled { "off".to_string() } else { gocompat::format_duration(auto.after) };
        let result = update_settings(self.settings.as_deref(), |s| s.auto_stop = saved);
        self.set_last_err(result.err());
        self.kick();
    }

    fn is_busy(&self, name: &str) -> bool {
        self.busy.lock().unwrap_or_else(PoisonError::into_inner).contains(name)
    }

    fn set_busy(&self, name: &str, busy: bool) {
        let mut set = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        if busy {
            set.insert(name.to_string());
        } else {
            set.remove(name);
        }
    }

    fn set_last_err(&self, err: Option<Error>) {
        *self.last_err.lock().unwrap_or_else(PoisonError::into_inner) = err.map(|e| e.to_string()).unwrap_or_default();
    }

    fn last_err(&self) -> String {
        self.last_err.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// The poll loop's state. The menu bar enforces auto-stop for every profile
/// while it runs; the TUI defers to it (see Model::track_idle).
pub struct Poller {
    shared: Arc<Shared>,
    idle: IdleTracker,
    sig: Option<String>,
    dim: Option<bool>,
}

impl Poller {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared, idle: IdleTracker::default(), sig: None, dim: None }
    }

    pub fn sync(&mut self, now: Instant) -> Frame {
        let listed = self.shared.backend.profiles();
        let list_err = listed.as_ref().err().map(ToString::to_string);
        let profiles = listed.unwrap_or_default();
        let dim = menubar_running(&profiles) == 0;
        let dim_changed = (self.dim != Some(dim)).then_some(dim);
        self.dim = Some(dim);

        let auto = AutoStopState::resolve(self.shared.settings.as_deref(), &self.shared.env_auto_stop);
        self.auto_stop_check(&profiles, &auto, now);
        let countdowns: HashMap<String, String> = profiles
            .iter()
            .filter(|_| auto.enabled)
            .filter_map(|p| {
                let remaining = self.idle.remaining(&p.name, now, auto.after)?;
                Some((p.name.clone(), format!("auto-stop in {}", format_countdown(remaining))))
            })
            .collect();

        let mut sig = format!("{}auto:{}", menubar_signature(&profiles), auto.label());
        for p in &profiles {
            sig += &format!("{}={};", p.name, countdowns.get(&p.name).map_or("", String::as_str));
        }
        if let Some(err) = &list_err {
            sig += &format!("err:{err}");
        }
        let busy: Vec<String> =
            self.shared.busy.lock().unwrap_or_else(PoisonError::into_inner).iter().cloned().collect();
        let last_err = self.shared.last_err();
        sig += &format!("busy:{}lastErr:{last_err}", busy.join(","));

        let menu = (self.sig.as_ref() != Some(&sig)).then(|| {
            menu_entries(&profiles, list_err.as_deref(), &last_err, &auto, &countdowns, |name| {
                busy.iter().any(|b| b == name)
            })
        });
        self.sig = Some(sig);
        Frame { title: menubar_title(&profiles), dim: dim_changed, menu }
    }

    fn auto_stop_check(&mut self, profiles: &[Profile], auto: &AutoStopState, now: Instant) {
        if !auto.enabled {
            self.idle = IdleTracker::default();
            return;
        }
        for p in profiles {
            if !is_running(&p.status) || self.shared.is_busy(&p.name) {
                self.idle.clear(&p.name);
                continue;
            }
            let Ok(containers) = self.shared.backend.containers(&p.name) else {
                self.idle.clear(&p.name);
                continue;
            };
            let active = containers.iter().any(|c| is_active(&c.state));
            if self.idle.observe(&p.name, active, now, auto.after) {
                self.shared.profile_action(&p.name, "stop");
            }
        }
    }
}

pub fn menu_entries(
    profiles: &[Profile],
    list_err: Option<&str>,
    last_err: &str,
    auto: &AutoStopState,
    countdowns: &HashMap<String, String>,
    is_busy: impl Fn(&str) -> bool,
) -> Vec<MenuEntry> {
    use MenuEntry::{Item, Label, Separator, Submenu};
    let mut menu = vec![Label(crate::NAME.into()), Separator];
    if let Some(err) = list_err {
        menu.extend([Label(truncate(&format!("colima unavailable: {err}"), 70)), Separator]);
    }
    if !last_err.is_empty() {
        menu.extend([Label(truncate(&format!("error: {last_err}"), 70)), Separator]);
    }
    let controls = |p: &Profile| -> Vec<MenuEntry> {
        let action = |label: &str, command| Item(label.into(), MenuAction::Profile { name: p.name.clone(), command });
        if is_busy(&p.name) {
            vec![Label("Working…".into())]
        } else if is_running(&p.status) {
            vec![action("Stop", "stop"), action("Restart", "restart")]
        } else {
            vec![action("Start", "start")]
        }
    };
    let profile_rows = |p: &Profile| -> Vec<MenuEntry> {
        let mut rows = vec![Label(menubar_profile_details(p))];
        rows.extend(countdowns.get(&p.name).map(|c| Label(c.clone())));
        rows.extend(controls(p));
        rows
    };
    if let [p] = profiles {
        menu.push(Label(menubar_profile_line(p)));
        menu.extend(profile_rows(p));
    } else {
        menu.extend(profiles.iter().map(|p| Submenu(menubar_profile_line(p), profile_rows(p))));
    }
    if !profiles.is_empty() {
        menu.push(Separator);
    }
    if auto.env || auto.err.is_some() {
        menu.push(Label(auto.label()));
    } else {
        menu.push(Item(auto.label(), MenuAction::ToggleAutoStop(auto.clone())));
    }
    menu.extend([Separator, Item("Open".into(), MenuAction::Open), Item("Quit".into(), MenuAction::Quit)]);
    menu
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeBackend, container, profile};
    use std::sync::mpsc;

    const MINUTE: Duration = Duration::from_secs(60);

    #[test]
    fn title_counts_running_profiles() {
        for (profiles, want) in [
            (vec![], ""),
            (vec![profile("default", "Stopped")], ""),
            (vec![profile("default", "Running")], ""),
            (vec![profile("a", "Running"), profile("b", "running")], "2"),
            (vec![profile("a", "Running"), profile("b", "Stopped")], ""),
        ] {
            assert_eq!(menubar_title(&profiles), want);
        }
    }

    #[test]
    fn dims_icon_alpha() {
        assert_eq!(dimmed_rgba(&[9, 9, 9, 255, 9, 9, 9, 0]), [0, 0, 0, 102, 0, 0, 0, 0]);
    }

    #[test]
    fn profile_rows() {
        assert_eq!(menubar_profile_line(&profile("default", "Running")), "default — Running");
        assert_eq!(menubar_profile_line(&profile("work", "")), "work — Unknown");
        let p = Profile { cpus: 2, memory: 2 << 30, disk: 60 << 30, arch: "aarch64".into(), ..profile("default", "") };
        assert_eq!(menubar_profile_details(&p), "2 cpu · 2.0g ram · 60g disk · aarch64");
    }

    #[test]
    fn signature_tracks_state() {
        let make = |status| Profile { cpus: 2, memory: 1024, disk: 2048, ..profile("default", status) };
        assert_eq!(menubar_signature(&[make("Running")]), menubar_signature(&[make("Running")]));
        assert_ne!(menubar_signature(&[make("Running")]), menubar_signature(&[make("Stopped")]));
        assert_eq!(menubar_signature(&[]), "");
    }

    #[test]
    fn idle_tracker_observe() {
        let mut tracker = IdleTracker::default();
        let now = Instant::now();
        let after = 30 * MINUTE;
        assert!(!tracker.observe("default", false, now, after), "first idle observation must arm, not fire");
        assert!(tracker.remaining("default", now, after).is_some());
        assert!(!tracker.observe("default", false, now + 29 * MINUTE, after));
        assert!(tracker.observe("default", false, now + 31 * MINUTE, after));
        assert!(!tracker.observe("default", false, now + 31 * MINUTE, after), "fired twice for one window");
    }

    #[test]
    fn idle_tracker_activity_and_clear_reset() {
        let mut tracker = IdleTracker::default();
        let now = Instant::now();
        tracker.observe("default", false, now, MINUTE);
        tracker.observe("other", false, now, MINUTE);
        assert!(!tracker.observe("default", true, now + 2 * MINUTE, MINUTE), "activity must reset, not fire");
        tracker.clear("other");
        assert!(!tracker.observe("default", false, now + 3 * MINUTE, MINUTE));
        assert!(tracker.remaining("other", now, MINUTE).is_none());
    }

    #[test]
    fn auto_stop_labels() {
        let state = |after: u32, enabled, env, err: Option<&str>| AutoStopState {
            after: after * MINUTE,
            enabled,
            env,
            err: err.map(String::from),
        };
        for (auto, want) in [
            (state(30, true, false, None), "Disable auto-stop"),
            (state(30, false, false, None), "Enable auto-stop"),
            (state(45, true, true, None), "idle auto-stop: 45m (COLIMUI_AUTO_STOP)"),
            (state(30, false, true, None), "idle auto-stop: off (COLIMUI_AUTO_STOP)"),
            (state(0, false, false, Some("boom")), "idle auto-stop: invalid setting"),
        ] {
            assert_eq!(auto.label(), want);
        }
    }

    #[test]
    fn resolve_follows_settings_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("colimui").join("config.json");
        let auto = AutoStopState::resolve(Some(&path), "");
        assert!(auto.enabled && auto.after == 30 * MINUTE && !auto.env && auto.err.is_none(), "{auto:?}");
        update_settings(Some(&path), |s| s.auto_stop = "off".into()).unwrap();
        assert!(!AutoStopState::resolve(Some(&path), "").enabled);
        let auto = AutoStopState::resolve(Some(&path), "45m");
        assert!(auto.enabled && auto.after == 45 * MINUTE && auto.env);
        let auto = AutoStopState::resolve(Some(&path), "nonsense");
        assert!(auto.err.is_some() && !auto.enabled);
    }

    #[test]
    fn applescript_quoting() {
        assert_eq!(applescript_quote("plain"), r#""plain""#);
        assert_eq!(applescript_quote(r#"with "quotes""#), r#""with \"quotes\"""#);
        assert_eq!(applescript_quote(r"back\slash"), r#""back\\slash""#);
    }

    fn enabled() -> AutoStopState {
        AutoStopState { after: 30 * MINUTE, enabled: true, env: false, err: None }
    }

    #[test]
    fn single_profile_menu_is_flat() {
        let countdowns = HashMap::from([("default".to_string(), "auto-stop in 12m".to_string())]);
        let menu = menu_entries(&[profile("default", "Running")], None, "", &enabled(), &countdowns, |_| false);
        let stop = MenuAction::Profile { name: "default".into(), command: "stop" };
        assert_eq!(menu[2], MenuEntry::Label("default — Running".into()));
        assert!(menu.contains(&MenuEntry::Label("auto-stop in 12m".into())));
        assert!(menu.contains(&MenuEntry::Item("Stop".into(), stop)));
        assert!(menu.contains(&MenuEntry::Item("Disable auto-stop".into(), MenuAction::ToggleAutoStop(enabled()))));
        assert_eq!(menu.last(), Some(&MenuEntry::Item("Quit".into(), MenuAction::Quit)));
    }

    #[test]
    fn multi_profile_menu_uses_submenus_and_shows_errors() {
        let profiles = [profile("default", "Running"), profile("work", "Stopped")];
        let pinned = AutoStopState { env: true, ..enabled() };
        let menu = menu_entries(&profiles, Some("boom"), "last", &pinned, &HashMap::new(), |name| name == "default");
        assert_eq!(menu[2], MenuEntry::Label("colima unavailable: boom".into()));
        assert_eq!(menu[4], MenuEntry::Label("error: last".into()));
        let MenuEntry::Submenu(label, rows) = &menu[6] else { panic!("{menu:?}") };
        assert_eq!((label.as_str(), rows.last()), ("default — Running", Some(&MenuEntry::Label("Working…".into()))));
        let MenuEntry::Submenu(_, rows) = &menu[7] else { panic!("{menu:?}") };
        assert!(matches!(rows.last(), Some(MenuEntry::Item(label, _)) if label == "Start"));
        assert!(menu.contains(&MenuEntry::Label("idle auto-stop: 30m (COLIMUI_AUTO_STOP)".into())));
    }

    #[test]
    fn poller_auto_stops_idle_profiles_once_and_skips_unchanged_menus() {
        let backend = FakeBackend::new();
        backend.state().profiles = vec![profile("default", "Running"), profile("busy", "Running")];
        backend.state().containers = Vec::new();
        let dir = tempfile::tempdir().unwrap();
        let (kick, _kicks) = mpsc::sync_channel(1);
        let shared = Shared::new(backend.clone(), Some(dir.path().join("config.json")), String::new(), kick);
        let mut poller = Poller::new(shared);
        let start = Instant::now();
        let first = poller.sync(start);
        assert_eq!((first.title.as_str(), first.dim), ("2", Some(false)));
        assert!(first.menu.is_some());
        let again = poller.sync(start);
        assert!(again.menu.is_none() && again.dim.is_none(), "unchanged state rebuilt the menu");
        poller.sync(start + 31 * MINUTE);
        let deadline = Instant::now() + Duration::from_secs(5);
        while backend.state().action_calls < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(backend.state().action_calls, 2, "both idle profiles should stop once");
        backend.state().containers = vec![container("id", "web", "running")];
        poller.sync(start + 90 * MINUTE);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(backend.state().action_calls, 2);
    }
}
