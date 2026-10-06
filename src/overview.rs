//! The Docker usage overview popup and reclaimable-storage cleanup.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::ansi;
use crate::backend::{is_running, nullable};
use crate::error::Error;
use crate::gocompat;
use crate::model::{Cmd, Model, Msg};
use crate::stats::{ContainerStats, STATS_INTERVAL};
use crate::view::{
    ACCENT, ERROR, LOG_HEADING, MUTED_STYLE, SELECTED, STATUS, TITLE, human_bytes, popup, sanitize_text,
};

const STORAGE_REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct StorageRow {
    #[serde(default, deserialize_with = "nullable")]
    pub r#type: String,
    #[serde(default, deserialize_with = "nullable")]
    pub size: String,
    #[serde(default, deserialize_with = "nullable")]
    pub reclaimable: String,
}

#[derive(Clone, Debug)]
pub struct StorageSample {
    pub profile: String,
    pub rows: Vec<StorageRow>,
    pub err: Option<String>,
    pub at: Instant,
}

pub struct CleanupMsg {
    pub profile: String,
    pub err: Option<String>,
}

pub fn parse_storage(output: &[u8]) -> Result<Vec<StorageRow>, Error> {
    let mut rows = Vec::new();
    for line in output.trim_ascii().split(|&b| b == b'\n') {
        if line.trim_ascii().is_empty() {
            continue;
        }
        let row: StorageRow = serde_json::from_str(&gocompat::decode_bytes(line))?;
        if row.r#type.is_empty() || row.size.is_empty() {
            return Err(Error::invalid("incomplete storage response"));
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return Err(Error::invalid("empty storage response"));
    }
    Ok(rows)
}

/// Parses docker's human byte sizes ("1.5GiB", "48.65MB").
pub fn usage_bytes(value: &str) -> Result<f64, Error> {
    let value = value.trim();
    let digits = value.bytes().take_while(|b| b.is_ascii_digit() || *b == b'.').count();
    let number: f64 = value[..digits]
        .parse()
        .ok()
        .filter(|n: &f64| n.is_finite())
        .ok_or_else(|| Error::invalid(format!("invalid byte size {value:?}")))?;
    let unit = value[digits..].trim();
    let factor = match unit {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return Err(Error::invalid(format!("unknown byte unit {unit:?}"))),
    };
    Ok(number * factor)
}

/// Sums CPU percent and used memory bytes across samples.
pub fn stats_totals(values: &[ContainerStats]) -> Result<(f64, f64), Error> {
    let (mut cpu, mut memory) = (0.0, 0.0);
    for value in values {
        let c: f64 = value
            .cpu
            .trim()
            .trim_end_matches('%')
            .parse()
            .ok()
            .filter(|c: &f64| *c >= 0.0 && c.is_finite())
            .ok_or_else(|| Error::invalid("invalid CPU sample"))?;
        let (used, _) = value.memory.split_once('/').ok_or_else(|| Error::invalid("invalid memory sample"))?;
        cpu += c;
        memory += usage_bytes(used)?;
    }
    Ok((cpu, memory))
}

impl Model {
    pub fn cleanup_cmd(&mut self) -> Option<Cmd> {
        if self.cleanup_running {
            return None;
        }
        let profile = self.current_profile_name();
        self.cleanup_running = true;
        let backend = Arc::clone(&self.backend);
        Some(Cmd::run(move || {
            let err = backend.cleanup(&profile).err().map(|err| err.to_string());
            Msg::Cleanup(CleanupMsg { profile, err })
        }))
    }

    pub fn cleanup_summary(&self) -> Vec<String> {
        let current = self.current_profile_name();
        let Some(storage) = self.storage.as_ref().filter(|s| s.profile == current && s.err.is_none()) else {
            return vec!["Docker will identify reclaimable resources when cleanup runs.".to_string()];
        };
        let lines: Vec<String> = storage
            .rows
            .iter()
            .filter(|row| !row.reclaimable.is_empty() && !row.reclaimable.starts_with("0B"))
            .map(|row| format!("{}: {} reclaimable", sanitize_text(&row.r#type), sanitize_text(&row.reclaimable)))
            .collect();
        if lines.is_empty() { vec!["Docker reports no reclaimable storage.".to_string()] } else { lines }
    }

    pub fn poll_storage(&mut self) -> Option<Cmd> {
        if !self.usage_overview || self.storage_busy {
            return None;
        }
        let profile = self.current_profile().filter(|p| is_running(&p.status))?.name.clone();
        let fresh = self.storage_requested.is_some_and(|at| at.elapsed() < STORAGE_REFRESH);
        if self.storage.as_ref().is_some_and(|s| s.profile == profile) && fresh {
            return None;
        }
        self.storage_busy = true;
        self.storage_requested = Some(Instant::now());
        let backend = Arc::clone(&self.backend);
        Some(Cmd::run(move || {
            let (rows, err) = match backend.storage(&profile) {
                Ok(rows) => (rows, None),
                Err(err) => (Vec::new(), Some(err.to_string())),
            };
            Msg::Storage(StorageSample { profile, rows, err, at: Instant::now() })
        }))
    }

    pub fn render_usage_overview(&self) -> String {
        let width = 78.min(self.width as i64 - 4).max(0) as usize;
        let content_width = width.saturating_sub(4).max(1);
        let mut lines = vec![
            TITLE.render("docker usage overview"),
            MUTED_STYLE.render(&format!("profile: {}", sanitize_text(&self.current_profile_name()))),
            String::new(),
        ];
        match self.current_profile().filter(|p| is_running(&p.status)) {
            None => lines.push(STATUS.render("colima is stopped or unavailable.")),
            Some(p) => {
                lines.push(format!(
                    "vm allocated: {} cpu · {} ram · {} disk",
                    p.cpus,
                    human_bytes(p.memory),
                    human_bytes(p.disk)
                ));
                lines.push(match self.overall.as_ref().filter(|s| s.profile == p.name) {
                    None => MUTED_STYLE.render("containers: loading…"),
                    Some(sample) if sample.err.is_some() => ERROR.render(&format!(
                        "containers unavailable: {}",
                        sanitize_text(sample.err.as_deref().unwrap_or_default())
                    )),
                    Some(sample) if sample.at.elapsed() > 3 * STATS_INTERVAL => {
                        STATUS.render("containers: stale; waiting for update")
                    }
                    Some(sample) => match stats_totals(&sample.all) {
                        Err(err) => {
                            ERROR.render(&format!("container totals unavailable: {}", sanitize_text(&err.to_string())))
                        }
                        Ok((cpu, memory)) => {
                            format!("{} running · cpu {cpu:.2}% · ram {}", sample.all.len(), human_bytes(memory as i64))
                        }
                    },
                });
                lines.push(MUTED_STYLE.render("cpu: 100% = one core; excludes vm/docker overhead."));
                lines.push(String::new());
                lines.push(overview_storage_header());
                match self.storage.as_ref().filter(|s| s.profile == p.name) {
                    None => lines.push(MUTED_STYLE.render("loading storage…")),
                    Some(storage) if storage.err.is_some() => lines.push(ERROR.render(&format!(
                        "storage unavailable: {}",
                        sanitize_text(storage.err.as_deref().unwrap_or_default())
                    ))),
                    Some(storage) => {
                        for row in &storage.rows {
                            lines.push(format!(
                                "{:<22} {:<10} {}",
                                overview_text(&row.r#type),
                                overview_text(&row.size),
                                overview_text(&row.reclaimable)
                            ));
                        }
                        lines.push(MUTED_STYLE.render(&format!(
                            "storage sampled {}s ago (refreshes every 30s).",
                            storage.at.elapsed().as_secs()
                        )));
                    }
                }
                lines.push(MUTED_STYLE.render("storage categories may share data; not host disk use."));
            }
        }
        if self.cleanup_running {
            lines.extend([
                String::new(),
                LOG_HEADING.render("cleanup"),
                STATUS.render("cleaning up reclaimable storage…"),
            ]);
        } else {
            lines.extend([String::new(), LOG_HEADING.render("keyboard shortcuts"), overview_footer()]);
        }
        let mut lines: Vec<String> = lines.iter().map(|line| ansi::truncate(line, content_width, "")).collect();
        // Keep the close hint visible even in a short terminal.
        let max_lines = self.height as i64 - 6;
        if lines.len() as i64 > max_lines {
            let footer = lines.pop().unwrap_or_default();
            lines.truncate((max_lines - 1).max(0) as usize);
            lines.push(footer);
        }
        popup(width, ACCENT).render(&lines.join("\n"))
    }
}

fn overview_storage_header() -> String {
    format!(
        "{}{}{}{}{}",
        LOG_HEADING.render("docker storage"),
        " ".repeat(9),
        MUTED_STYLE.render("used"),
        " ".repeat(8),
        MUTED_STYLE.render("reclaimable")
    )
}

fn overview_text(value: &str) -> String {
    sanitize_text(value).to_lowercase()
}

fn overview_footer() -> String {
    format!(
        "{}{}{}{}{}",
        SELECTED.render("c"),
        MUTED_STYLE.render(" clean up reclaimable storage"),
        MUTED_STYLE.render("  ·  "),
        SELECTED.render("esc / q / u"),
        MUTED_STYLE.render(" close")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::{block_height, block_width, strip};
    use crate::stats::StatsSample;
    use crate::testutil::{FakeBackend, container, model, model_with, profile, run_one};
    use crate::update::shortcut_key;

    fn stats(cpu: &str, memory: &str) -> ContainerStats {
        ContainerStats { cpu: cpu.into(), memory: memory.into(), ..ContainerStats::default() }
    }

    #[test]
    fn overview_totals() {
        let (cpu, mem) = stats_totals(&[stats("125.5%", "1MiB / 2GiB"), stats("2.5%", "1MB / 2GiB")]).unwrap();
        assert_eq!((cpu, mem), (128.0, 2_048_576.0));
        assert_eq!(stats_totals(&[]).unwrap(), (0.0, 0.0));
        for bad in [stats("NaN", "1MB / 2GB"), stats("2%", "bad"), stats("2%", "1PB / 2PB")] {
            assert!(stats_totals(&[bad]).is_err(), "invalid sample accepted");
        }
    }

    #[test]
    fn overview_parsing() {
        assert!(crate::stats::parse_all_stats(b"").unwrap().is_empty());
        assert!(
            crate::stats::parse_all_stats(br#"{"CPUPerc":"1%","MemUsage":"1MB / 2GB","NetIO":"0B / 0B"}"#).is_err()
        );
        let rows = parse_storage(b"{\"Type\":\"Images\",\"Size\":\"1GB\",\"Reclaimable\":\"0B\"}\n{\"Type\":\"Containers\",\"Size\":\"0B\"}\n").unwrap();
        assert_eq!(rows.len(), 2);
        for bad in ["", "{}", "null", "bad"] {
            assert!(parse_storage(bad.as_bytes()).is_err(), "bad storage accepted: {bad:?}");
        }
    }

    #[test]
    fn overview_polling_and_filtering() {
        let backend = FakeBackend::new();
        let mut m = model_with(&backend);
        m.profiles = vec![profile("dev", "Running")];
        m.search_query = "does-not-match".into(); // Summary must ignore list filters and selection.
        let cmd = m.poll_stats().expect("stats poll missing");
        assert!(m.poll_stats().is_none(), "stats poll overlap");
        m.update(run_one(cmd));
        assert_eq!(m.overall.as_ref().map(|s| s.all.len()), Some(1), "summary missing");
        assert!(m.poll_storage().is_none(), "storage polled while closed");
        m.usage_overview = true;
        let cmd = m.poll_storage().expect("storage poll missing");
        assert!(m.poll_storage().is_none(), "storage poll overlap");
        m.update(run_one(cmd));
        assert!(m.poll_storage().is_none(), "storage not throttled");
        m.update(Msg::Stats(StatsSample { profile: "other".into(), all: Vec::new(), err: None, at: Instant::now() }));
        assert_eq!(m.overall.as_ref().unwrap().profile, "dev", "wrong profile sample accepted");
        m.containers = vec![container("a", "", "running")];
        m.search_query.clear();
        assert!(m.resource_lines(80).concat().contains("50%"), "selected stats not shared");
        for (width, height) in [(80, 24), (60, 16)] {
            (m.width, m.height) = (width, height);
            let view = m.render_usage_overview();
            assert!(block_width(&view) <= width && block_height(&view) <= height, "overview overflow");
            assert!(view.contains("close"), "close hint missing");
        }
        m.storage_requested = Instant::now().checked_sub(Duration::from_secs(31));
        assert!(m.poll_storage().is_some(), "storage never refreshed");
    }

    #[test]
    fn overview_menu_entry() {
        let mut m = model();
        assert!(m.action_menu_items().iter().any(|item| item.shortcut == "u"), "overview absent from Actions");
        m.key(shortcut_key("u"));
        assert!(m.usage_overview, "shortcut did not open");
        m.key(shortcut_key("q"));
        assert!(!m.usage_overview, "q did not close");
    }

    fn overview_model(rows: Vec<StorageRow>) -> Model {
        let mut m = model();
        (m.width, m.height) = (100, 30);
        m.profiles =
            vec![crate::model::Profile { cpus: 2, memory: 2 << 30, disk: 100 << 30, ..profile("default", "Running") }];
        m.storage = Some(StorageSample { profile: "default".into(), rows, err: None, at: Instant::now() });
        m.overall = Some(StatsSample { profile: "default".into(), all: Vec::new(), err: None, at: Instant::now() });
        m
    }

    fn row(kind: &str, size: &str, reclaimable: &str) -> StorageRow {
        StorageRow { r#type: kind.into(), size: size.into(), reclaimable: reclaimable.into() }
    }

    #[test]
    fn overview_uses_actions_menu_header_and_footer_style() {
        let m = overview_model(vec![row("Images", "1GB", "1GB (100%)")]);
        let view = strip(&m.render_usage_overview());
        let lines: Vec<&str> = view.split('\n').collect();
        let title = lines.iter().find(|l| l.contains("docker usage overview")).unwrap();
        assert!(title.find("docker usage overview").unwrap() <= 6, "title is not aligned: {title:?}");
        let footer = lines.iter().position(|l| l.contains("clean up reclaimable storage")).unwrap();
        assert!(lines[footer - 1].contains("keyboard shortcuts") && lines[footer].contains("esc / q / u"));
    }

    #[test]
    fn overview_and_actions_menu_use_the_same_popup_width() {
        let mut m = model();
        (m.width, m.height) = (100, 30);
        assert_eq!(block_width(&m.render_action_menu()), block_width(&m.render_usage_overview()));
    }

    #[test]
    fn overview_lowercases_docker_display_text() {
        let m = overview_model(vec![row("Local Volumes", "48.65MB", "48.65MB (100%)")]);
        let view = strip(&m.render_usage_overview());
        for unexpected in ["Docker usage", "VM allocated", "CPU", "RAM", "Local Volumes", "48.65MB"] {
            assert!(!view.contains(unexpected), "overview contains {unexpected:?}: {view}");
        }
        for expected in ["docker usage overview", "vm allocated", "cpu", "ram", "local volumes", "48.65mb"] {
            assert!(view.contains(expected), "overview is missing {expected:?}: {view}");
        }
    }

    #[test]
    fn cleanup_requires_confirmation_and_refreshes_storage() {
        let backend = FakeBackend::new();
        let mut m = model_with(&backend);
        (m.width, m.height) = (100, 30);
        m.profiles = vec![profile("default", "Running")];
        m.storage = Some(StorageSample {
            profile: "default".into(),
            rows: vec![row("Images", "", "429.1MB (99%)"), row("Local Volumes", "", "48.65MB (100%)")],
            err: None,
            at: Instant::now(),
        });
        let cmd = m.key(shortcut_key("c"));
        assert!(cmd.is_some() && m.usage_overview && m.confirm_cleanup && backend.state().cleanups == 0);
        let view = m.render_cleanup_confirmation();
        assert!(
            view.contains("Images: 429.1MB")
                && view.contains("Local Volumes: 48.65MB")
                && view.contains("cannot be recovered"),
            "{view}"
        );
        let cmd = m.key(shortcut_key("y")).expect("confirmation did not start cleanup");
        assert!(!m.confirm_cleanup && m.cleanup_running && backend.state().cleanups == 0);
        let msg = run_one(cmd);
        assert_eq!(backend.state().cleanups, 1);
        let cmd = m.update(msg);
        assert!(cmd.is_some() && !m.cleanup_running && m.status == "cleanup complete" && m.storage.is_none());
    }

    #[test]
    fn cleanup_cancellation_and_failure() {
        let backend = FakeBackend::new();
        backend.state().cleanup_err = Some("daemon failed".into());
        let mut m = model_with(&backend);
        m.profiles = vec![profile("default", "Running")];
        (m.usage_overview, m.confirm_cleanup) = (true, true);
        assert!(m.key(shortcut_key("n")).is_none() && !m.confirm_cleanup && backend.state().cleanups == 0);
        m.confirm_cleanup = true;
        let cmd = m.key(shortcut_key("y")).unwrap();
        m.update(run_one(cmd));
        assert!(m.status == "cleanup failed" && m.err.is_some() && !m.cleanup_running);
    }
}
