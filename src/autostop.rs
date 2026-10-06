//! Idle auto-stop: stop the current profile once it has run with no active
//! containers for the configured window.

use std::time::Duration;

use crate::backend::is_running;
use crate::error::Error;
use crate::gocompat;
use crate::model::{Cmd, Model};
use crate::settings::Settings;
use crate::tea::batch;
use crate::update::spinner_tick;

pub const AUTO_STOP_DEFAULT: Duration = Duration::from_secs(30 * 60);
pub const AUTO_STOP_ENV: &str = "COLIMUI_AUTO_STOP";
const MINUTE: Duration = Duration::from_secs(60);

/// Picks the idle window: COLIMUI_AUTO_STOP wins for this run, then the saved
/// config, then the default. `path` only names the config file in errors.
pub fn resolve_auto_stop(env_value: &str, saved: &Settings, path: &str) -> Result<(Duration, bool), Error> {
    let value = env_value.trim();
    if !value.is_empty() {
        return parse_auto_stop(value).map_err(|err| err.context(format!("invalid {AUTO_STOP_ENV} {value:?}")));
    }
    if saved.auto_stop.is_empty() {
        return Ok((AUTO_STOP_DEFAULT, true));
    }
    parse_auto_stop(&saved.auto_stop)
        .map_err(|err| err.context(format!("invalid auto_stop {:?} in {path}", saved.auto_stop)))
}

/// Accepts "off"/"0"/"false" or a duration of at least a minute; a disabled
/// setting keeps the default window so the toggle stays usable.
fn parse_auto_stop(value: &str) -> Result<(Duration, bool), Error> {
    if matches!(value.to_lowercase().as_str(), "off" | "0" | "false") {
        return Ok((AUTO_STOP_DEFAULT, false));
    }
    match gocompat::parse_duration(value) {
        Some(after) if after >= MINUTE => Ok((after, true)),
        _ => Err(Error::invalid("use a duration of 1m or more (e.g. 45m, 2h) or \"off\"")),
    }
}

/// States that hold off the idle auto-stop; paused containers count because
/// they would not survive a VM stop.
pub fn is_active(state: &str) -> bool {
    matches!(state.to_lowercase().as_str(), "running" | "restarting" | "paused")
}

pub fn format_countdown(d: Duration) -> String {
    if d >= MINUTE {
        let minutes = d.as_secs() / 60;
        let rounded = if (d - MINUTE * minutes as u32) * 2 < MINUTE { minutes } else { minutes + 1 };
        format!("{rounded}m")
    } else {
        format!("{}s", d.as_secs())
    }
}

impl Model {
    /// Runs after each applied refresh: once the current profile has run with
    /// no active containers for `auto_stop_after`, stop it like a manual `x`.
    /// Errors and in-flight actions reset the timer rather than risk a bad stop.
    pub fn track_idle(&mut self) -> Option<Cmd> {
        let name = match self.current_profile() {
            Some(p) if self.auto_stop && is_running(&p.status) && self.err.is_none() && !self.has_active_actions() => {
                p.name.clone()
            }
            _ => {
                self.clear_idle();
                return None;
            }
        };
        // The menu bar process enforces auto-stop for every profile while it
        // is alive (and keeps doing so after the TUI exits); only one of the
        // two may dispatch stops, so the TUI stands down.
        if (self.menubar_alive)() {
            self.clear_idle();
            return None;
        }
        if self.idle_profile != name {
            self.clear_idle();
            self.idle_profile = name.clone();
        }
        if self.containers.iter().any(|c| is_active(&c.state)) {
            self.idle_since = None;
            return None;
        }
        let now = self.clock();
        let Some(since) = self.idle_since else {
            self.idle_since = Some(now);
            return None;
        };
        if now.saturating_duration_since(since) < self.auto_stop_after {
            return None;
        }
        self.clear_idle();
        self.status = format!("auto-stopping {name} (idle {})", format_countdown(self.auto_stop_after));
        let stop = self.action_cmd(&name, "auto-stop", "colima", &["stop", "--profile", &name]);
        batch([Some(stop), Some(spinner_tick())])
    }

    pub fn clear_idle(&mut self) {
        self.idle_profile.clear();
        self.idle_since = None;
    }

    pub fn idle_remaining(&self) -> Option<Duration> {
        if !self.auto_stop {
            return None;
        }
        let since = self.idle_since?;
        Some(self.auto_stop_after.saturating_sub(self.clock().saturating_duration_since(since)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Container, Msg, RefreshMsg};
    use crate::settings::load_settings;
    use crate::tea::Key;
    use crate::testutil::{Clock, FakeBackend, container, model_with, profile, run_all};
    use std::sync::Arc;

    #[test]
    fn resolve_auto_stop_sources() {
        let min = MINUTE;
        for (name, env, saved, want) in [
            ("defaults", "", "", Some((AUTO_STOP_DEFAULT, true))),
            ("env off", "off", "", Some((AUTO_STOP_DEFAULT, false))),
            ("env zero", "0", "", Some((AUTO_STOP_DEFAULT, false))),
            ("env custom", "45m", "", Some((45 * min, true))),
            ("env below minimum", "30s", "", None),
            ("env garbage", "soon", "", None),
            ("saved off", "", "off", Some((AUTO_STOP_DEFAULT, false))),
            ("saved custom", "", "2h", Some((120 * min, true))),
            ("saved go format", "", "30m0s", Some((30 * min, true))),
            ("saved garbage value", "", "soon", None),
            ("env beats saved", "off", "2h", Some((AUTO_STOP_DEFAULT, false))),
        ] {
            let saved = Settings { auto_stop: saved.into(), ..Settings::default() };
            let got = resolve_auto_stop(env, &saved, "config.json").ok();
            assert_eq!(got, want, "{name}");
        }
        let err = resolve_auto_stop("30s", &Settings::default(), "config.json").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid COLIMUI_AUTO_STOP \"30s\": use a duration of 1m or more (e.g. 45m, 2h) or \"off\""
        );
    }

    #[test]
    fn countdown_rounds_like_go() {
        assert_eq!(format_countdown(Duration::from_secs(30 * 60)), "30m");
        assert_eq!(format_countdown(Duration::from_secs(90)), "2m");
        assert_eq!(format_countdown(Duration::from_secs(89)), "1m");
        assert_eq!(format_countdown(Duration::from_secs(59)), "59s");
    }

    fn idle_model(backend: &Arc<FakeBackend>, clock: &Clock) -> Model {
        let mut m = model_with(backend);
        (m.width, m.height, m.auto_stop) = (100, 24, true);
        m.now = clock.source();
        m
    }

    fn running(profiles: Vec<crate::model::Profile>, profile_name: &str, containers: Vec<Container>) -> Msg {
        Msg::Refresh(RefreshMsg {
            profile_name: profile_name.into(),
            request_id: 0,
            profiles,
            containers,
            err: None,
            list_failed: false,
        })
    }

    fn idle() -> Msg {
        running(vec![profile("default", "Running")], "default", Vec::new())
    }

    #[test]
    fn idle_auto_stop_dispatches_after_threshold() {
        let backend = FakeBackend::new();
        let clock = Clock::new();
        let mut m = idle_model(&backend, &clock);
        assert!(m.update(idle()).is_none());
        assert!(m.view().contains("auto-stop in 30m"), "{}", m.view());
        clock.advance(29 * MINUTE);
        assert!(m.update(idle()).is_none());
        assert!(m.view().contains("auto-stop in 1m"), "{}", m.view());
        clock.advance(2 * MINUTE);
        let cmd = m.update(idle()).expect("threshold refresh dispatched nothing");
        assert!(m.has_active_profile_action() && m.status.contains("auto-stopping default"), "{}", m.status);
        run_all(cmd);
        let state = backend.state();
        assert_eq!(
            (state.action_command.as_str(), state.action_args.join(" ")),
            ("colima", "stop --profile default".to_string())
        );
        drop(state);
        // The in-flight stop must not be dispatched a second time.
        clock.advance(60 * MINUTE);
        assert!(m.update(idle()).is_none());
        assert_eq!(backend.state().action_calls, 1);
    }

    #[test]
    fn idle_auto_stop_defers_to_menubar() {
        let backend = FakeBackend::new();
        let clock = Clock::new();
        let mut m = idle_model(&backend, &clock);
        m.menubar_alive = Arc::new(|| true);
        assert!(m.update(idle()).is_none());
        assert!(!m.view().contains("auto-stop in"));
        clock.advance(60 * MINUTE);
        assert!(m.update(idle()).is_none());
        assert_eq!(backend.state().action_calls, 0);
    }

    #[test]
    fn idle_timer_resets() {
        let profiles = || vec![profile("default", "Running"), profile("dev", "Running")];
        type Case = (&'static str, Box<dyn Fn() -> Msg>);
        let cases: Vec<Case> = vec![
            (
                "running container",
                Box::new(move || running(profiles(), "default", vec![container("id", "web", "running")])),
            ),
            (
                "paused container",
                Box::new(move || running(profiles(), "default", vec![container("id", "web", "paused")])),
            ),
            (
                "refresh error",
                Box::new(move || {
                    Msg::Refresh(RefreshMsg {
                        profile_name: "default".into(),
                        request_id: 0,
                        profiles: profiles(),
                        containers: Vec::new(),
                        err: Some("daemon offline".into()),
                        list_failed: false,
                    })
                }),
            ),
            ("profile stopped", Box::new(|| running(vec![profile("default", "Stopped")], "default", Vec::new()))),
            ("profile switched", Box::new(move || running(profiles(), "dev", Vec::new()))),
        ];
        for (name, msg) in cases {
            let backend = FakeBackend::new();
            let clock = Clock::new();
            let mut m = idle_model(&backend, &clock);
            m.update(running(profiles(), "default", Vec::new()));
            if name == "profile switched" {
                m.profile_index = 1;
            }
            clock.advance(29 * MINUTE);
            m.update(msg());
            clock.advance(2 * MINUTE);
            m.update(msg());
            assert!(!m.has_active_profile_action() && backend.state().action_calls == 0, "auto-stop fired: {name}");
        }
    }

    fn reload_auto_stop(path: &std::path::Path) -> (bool, Duration) {
        let saved = load_settings(Some(path)).unwrap();
        let (after, enabled) = resolve_auto_stop("", &saved, "").unwrap();
        (enabled, after)
    }

    #[test]
    fn auto_stop_toggle_key_persists() {
        let backend = FakeBackend::new();
        let clock = Clock::new();
        let mut m = idle_model(&backend, &clock);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("colimui").join("config.json");
        m.settings_file = Some(path.clone());
        m.update(idle());
        assert!(m.idle_remaining().is_some(), "idle timer not armed");
        m.key(Key::runes("a"));
        assert!(!m.auto_stop && m.idle_remaining().is_none() && m.status == "idle auto-stop off", "{}", m.status);
        assert!(!reload_auto_stop(&path).0, "saved setting after toggle off");
        m.key(Key::runes("a"));
        assert!(m.auto_stop && m.status == "idle auto-stop on (30m)", "{}", m.status);
        assert_eq!(reload_auto_stop(&path), (true, AUTO_STOP_DEFAULT));
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"auto_stop\": \"30m0s\""), "{saved}");
    }

    #[test]
    fn auto_stop_toggle_reports_save_failure() {
        let backend = FakeBackend::new();
        let clock = Clock::new();
        let mut m = idle_model(&backend, &clock);
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        m.settings_file = Some(blocker.join("config.json"));
        m.key(Key::runes("a"));
        assert!(!m.auto_stop && m.err.is_some() && m.status.ends_with("(not saved)"), "{}", m.status);
    }

    #[test]
    fn auto_stop_toggle_refused_when_env_pinned() {
        let backend = FakeBackend::new();
        let clock = Clock::new();
        let mut m = idle_model(&backend, &clock);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("colimui").join("config.json");
        m.settings_file = Some(path.clone());
        m.auto_stop_pinned = true;
        m.key(Key::runes("a"));
        assert!(m.auto_stop && m.status == format!("idle auto-stop is set by {AUTO_STOP_ENV}"));
        assert!(!path.exists(), "pinned toggle must not write settings");
    }
}
