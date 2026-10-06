//! Container resource sampling via `docker stats` for the details pane and
//! the usage overview.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::backend::{is_running, nullable};
use crate::error::Error;
use crate::gocompat;
use crate::model::{Cmd, Model, Msg};
use crate::view::truncate;

pub const STATS_INTERVAL: Duration = Duration::from_secs(3);
pub const STATS_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ContainerStats {
    #[serde(rename = "ID", default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(rename = "CPUPerc", default, deserialize_with = "nullable")]
    pub cpu: String,
    #[serde(rename = "MemUsage", default, deserialize_with = "nullable")]
    pub memory: String,
    #[serde(rename = "NetIO", default, deserialize_with = "nullable")]
    pub network: String,
}

/// One `docker stats` sample of every running container in a profile.
#[derive(Clone, Debug)]
pub struct StatsSample {
    pub profile: String,
    pub all: Vec<ContainerStats>,
    pub err: Option<String>,
    pub at: Instant,
}

pub fn stats_tick() -> Cmd {
    Cmd::Tick(STATS_INTERVAL, Msg::StatsTick)
}

pub fn parse_stats(output: &[u8]) -> Result<ContainerStats, Error> {
    let value: ContainerStats = serde_json::from_str(&gocompat::decode_bytes(output))
        .map_err(|err| Error::Json(err).context("invalid stats response"))?;
    if value.cpu.trim().is_empty() || value.memory.trim().is_empty() || value.network.trim().is_empty() {
        return Err(Error::invalid("incomplete stats response"));
    }
    Ok(value)
}

pub fn parse_all_stats(output: &[u8]) -> Result<Vec<ContainerStats>, Error> {
    let mut values = Vec::new();
    for line in output.trim_ascii().split(|&b| b == b'\n') {
        if line.trim_ascii().is_empty() {
            continue;
        }
        let value = parse_stats(line)?;
        if value.id.is_empty() {
            return Err(Error::invalid("stats missing container ID"));
        }
        values.push(value);
    }
    Ok(values)
}

impl Model {
    pub fn poll_stats(&mut self) -> Option<Cmd> {
        if self.stats_busy {
            return None;
        }
        let profile = self.current_profile().filter(|p| is_running(&p.status))?.name.clone();
        self.stats_busy = true;
        let backend = Arc::clone(&self.backend);
        Some(Cmd::run(move || {
            let (all, err) = match backend.all_stats(&profile) {
                Ok(all) => (all, None),
                Err(err) => (Vec::new(), Some(err.to_string())),
            };
            Msg::Stats(StatsSample { profile, all, err, at: Instant::now() })
        }))
    }

    pub fn resource_lines(&self, width: usize) -> Vec<String> {
        let Some(c) = self.selected_container() else {
            return Vec::new();
        };
        if !is_running(&c.state) {
            return vec!["usage   stopped".to_string()];
        }
        let Some(sample) = self.overall.as_ref().filter(|s| s.profile == self.current_profile_name()) else {
            return vec!["usage   loading…".to_string()];
        };
        let value = sample.all.iter().find(|v| v.id == c.id);
        if let Some(err) = &sample.err {
            return vec![truncate(&format!("usage   {err}"), width)];
        }
        let Some(value) = value else {
            return vec!["usage   waiting for sample…".to_string()];
        };
        if sample.at.elapsed() > 3 * STATS_INTERVAL {
            return vec!["usage   stale; waiting for update".to_string()];
        }
        vec![
            truncate(&format!("cpu     {}", value.cpu), width),
            truncate(&format!("memory  {}", value.memory), width),
            truncate(&format!("net I/O {}", value.network), width),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::{block_height, block_width};
    use crate::model::Container;
    use crate::testutil::{container, model, profile};

    #[test]
    fn stats_parse() {
        let good = r#"{"CPUPerc":"1.2%","MemUsage":"12MiB / 2GiB","NetIO":"1MB / 2MB"}"#;
        assert_eq!(parse_stats(good.as_bytes()).unwrap().cpu, "1.2%");
        for bad in ["", "null", "{}", "garbage", &format!("{good}{good}")] {
            assert!(parse_stats(bad.as_bytes()).is_err(), "accepted {bad:?}");
        }
    }

    fn sample(profile: &str, stats: ContainerStats) -> StatsSample {
        StatsSample { profile: profile.into(), all: vec![stats], err: None, at: Instant::now() }
    }

    #[test]
    fn resource_lines_follow_the_selected_sample() {
        let mut m = model();
        m.profiles = vec![profile("default", "Running")];
        m.containers = vec![container("b", "b", "running")];
        assert_eq!(m.resource_lines(80), ["usage   loading…"]);
        let stats = ContainerStats {
            id: "b".into(),
            cpu: "1.2%".into(),
            memory: "12MiB / 2GiB".into(),
            network: "1MB / 2MB".into(),
        };
        m.overall = Some(sample("default", stats.clone()));
        let lines = m.resource_lines(80).join("\n");
        for want in ["1.2%", "12MiB / 2GiB", "1MB / 2MB"] {
            assert!(lines.contains(want), "{lines}");
        }
        m.overall = Some(StatsSample { err: Some("offline".into()), ..sample("default", stats.clone()) });
        assert!(m.resource_lines(80).concat().contains("offline"), "missing error");
        m.overall =
            Some(StatsSample { at: Instant::now() - Duration::from_secs(60), ..sample("default", stats.clone()) });
        assert!(m.resource_lines(80).concat().contains("stale"), "old sample looks live");
        m.overall = Some(sample("other", stats.clone()));
        assert!(m.resource_lines(80).concat().contains("loading"), "cross-profile sample displayed");
        m.overall = Some(sample("default", ContainerStats { id: "other".into(), ..stats }));
        assert!(m.resource_lines(80).concat().contains("waiting for sample"));
        m.containers[0].state = "exited".into();
        assert_eq!(m.resource_lines(80), ["usage   stopped"]);
    }

    #[test]
    fn stats_details_fit() {
        let mut m = model();
        m.containers = vec![Container { ports: "port ".repeat(40), ..container("a", "test", "running") }];
        let stats = ContainerStats {
            id: "a".into(),
            cpu: "1%".into(),
            memory: "12MiB / 2GiB".into(),
            network: "1MB / 2MB".into(),
        };
        m.overall = Some(sample("default", stats));
        let view = m.render_details(10, 53);
        assert!(
            block_height(&view) == 12 && block_width(&view) <= 53,
            "pane is {}x{}",
            block_width(&view),
            block_height(&view)
        );
        for label in ["cpu", "memory", "net I/O"] {
            assert!(view.contains(label), "missing {label}");
        }
    }
}
