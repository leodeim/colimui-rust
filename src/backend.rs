//! How colimuir talks to colima and docker: the Backend seam the model drives
//! and its implementation on top of the CLIs.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Deserializer};

use crate::error::Error;
use crate::gocompat;
use crate::logs::{self, LogReader};
use crate::model::{Container, Profile};
use crate::overview::{self, StorageRow};
use crate::process;
use crate::stats::{self, ContainerStats, STATS_TIMEOUT};

/// Bounds each profile/container listing so a wedged daemon cannot stall a
/// refresh or the menu bar poll loop.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(15);
const STORAGE_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How much history a log stream starts with: `from_start` loads everything,
/// `since` resumes after a timestamp, otherwise a short tail.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogRequest {
    pub follow: bool,
    pub from_start: bool,
    pub since: String,
}

pub trait Backend: Send + Sync {
    fn profiles(&self) -> Result<Vec<Profile>, Error>;
    fn containers(&self, profile: &str) -> Result<Vec<Container>, Error>;
    fn action(&self, profile: &str, command: &str, args: &[String]) -> Result<(), Error>;
    fn open_logs(&self, profile: &str, id: &str, req: &LogRequest) -> Result<Arc<LogReader>, Error>;
    fn shell(&self, profile: &str, id: &str) -> Command;
    fn all_stats(&self, profile: &str) -> Result<Vec<ContainerStats>, Error>;
    fn storage(&self, profile: &str) -> Result<Vec<StorageRow>, Error>;
    /// Removes only unused Docker resources: never a running container or
    /// data from a volume still attached to a container.
    fn cleanup(&self, profile: &str) -> Result<(), Error>;
}

pub fn docker_context(profile: &str) -> String {
    if profile.is_empty() || profile == "default" { "colima".to_string() } else { format!("colima-{profile}") }
}

pub fn is_running(status: &str) -> bool {
    status.to_lowercase() == "running"
}

/// Accepts JSON null where Go would leave the zero value.
pub fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

pub struct ExecBackend;

impl Backend for ExecBackend {
    fn profiles(&self) -> Result<Vec<Profile>, Error> {
        parse_profiles(&command_output("", LIST_TIMEOUT, "colima", &["list", "--json"])?)
    }

    fn containers(&self, profile: &str) -> Result<Vec<Container>, Error> {
        parse_containers(&command_output(profile, LIST_TIMEOUT, "docker", &["ps", "--all", "--format", "{{json .}}"])?)
    }

    fn action(&self, profile: &str, command: &str, args: &[String]) -> Result<(), Error> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        if command == "docker" {
            apply_docker_env(&mut cmd, profile);
        }
        process::combined_output(cmd)
    }

    fn open_logs(&self, profile: &str, id: &str, req: &LogRequest) -> Result<Arc<LogReader>, Error> {
        let mut cmd = Command::new("docker");
        cmd.args(["logs", "--timestamps"]);
        if !req.since.is_empty() {
            cmd.args(["--since", &req.since]);
        } else if !req.from_start {
            cmd.args(["--tail", "200"]);
        }
        if req.follow {
            cmd.arg("--follow");
        }
        cmd.arg(id);
        apply_docker_env(&mut cmd, profile);
        logs::start_log_reader(cmd)
    }

    fn shell(&self, profile: &str, id: &str) -> Command {
        let mut cmd = Command::new("docker");
        cmd.args(["exec", "-it", id, "sh", "-c", "command -v bash >/dev/null 2>&1 && exec bash || exec sh"]);
        apply_docker_env(&mut cmd, profile);
        cmd
    }

    fn all_stats(&self, profile: &str) -> Result<Vec<ContainerStats>, Error> {
        stats::parse_all_stats(&command_output(
            profile,
            STATS_TIMEOUT,
            "docker",
            &["stats", "--no-stream", "--format", "{{json .}}"],
        )?)
    }

    fn storage(&self, profile: &str) -> Result<Vec<StorageRow>, Error> {
        overview::parse_storage(&command_output(
            profile,
            STORAGE_TIMEOUT,
            "docker",
            &["system", "df", "--format", "{{json .}}"],
        )?)
    }

    fn cleanup(&self, profile: &str) -> Result<(), Error> {
        for args in [["system", "prune", "--all", "--force"], ["volume", "prune", "--all", "--force"]] {
            command_output(profile, CLEANUP_TIMEOUT, "docker", &args)
                .map_err(|err| err.context(format!("docker {}", args.join(" "))))?;
        }
        Ok(())
    }
}

/// Runs a bounded query; docker targets the profile's daemon.
fn command_output(profile: &str, timeout: Duration, program: &str, args: &[&str]) -> Result<Vec<u8>, Error> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if program == "docker" {
        apply_docker_env(&mut cmd, profile);
    }
    process::output(cmd, timeout)
}

fn apply_docker_env(cmd: &mut Command, profile: &str) {
    let (key, value) = DOCKER_HOSTS.env(profile);
    cmd.env(key, value);
}

static DOCKER_HOSTS: LazyLock<DockerHosts> = LazyLock::new(|| {
    DockerHosts::new(Box::new(|profile| {
        command_output("", LIST_TIMEOUT, "colima", &["status", "--json", "--profile", profile])
    }))
});

type StatusFn = Box<dyn Fn(&str) -> Result<Vec<u8>, Error> + Send + Sync>;

/// Points docker at each profile's socket as colima reports it, so a missing
/// or edited "colima" docker context cannot break colimuir. Sockets are cached
/// because colima derives them from the profile name alone; the named
/// context is the fallback while colima cannot report one.
pub struct DockerHosts {
    by_profile: Mutex<HashMap<String, String>>,
    status: StatusFn,
}

impl DockerHosts {
    pub fn new(status: StatusFn) -> Self {
        Self { by_profile: Mutex::new(HashMap::new()), status }
    }

    pub fn env(&self, profile: &str) -> (&'static str, String) {
        match self.host(profile) {
            Some(host) => ("DOCKER_HOST", host),
            None => ("DOCKER_CONTEXT", docker_context(profile)),
        }
    }

    fn host(&self, profile: &str) -> Option<String> {
        if let Some(host) = self.by_profile.lock().unwrap_or_else(PoisonError::into_inner).get(profile) {
            return Some(host.clone());
        }
        #[derive(Deserialize)]
        struct Status {
            #[serde(default, deserialize_with = "nullable")]
            docker_socket: String,
        }
        let output = (self.status)(profile).ok()?;
        let status: Status = serde_json::from_str(&gocompat::decode_bytes(&output)).ok()?;
        if status.docker_socket.is_empty() {
            return None;
        }
        self.by_profile
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(profile.to_string(), status.docker_socket.clone());
        Some(status.docker_socket)
    }
}

/// Accepts a JSON array, a single object, or one object per line; colima
/// prints the last form when several profiles exist.
fn parse_profiles(output: &[u8]) -> Result<Vec<Profile>, Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Listing {
        Many(Vec<Profile>),
        One(Profile),
    }
    let text = gocompat::decode_bytes(output);
    let mut profiles = Vec::new();
    for listing in serde_json::Deserializer::from_str(&text).into_iter::<Listing>() {
        match listing? {
            Listing::Many(many) => profiles.extend(many),
            Listing::One(one) => profiles.push(one),
        }
    }
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(profiles)
}

fn parse_containers(output: &[u8]) -> Result<Vec<Container>, Error> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Item {
        #[serde(rename = "ID", default, deserialize_with = "nullable")]
        id: String,
        #[serde(default, deserialize_with = "nullable")]
        names: String,
        #[serde(default, deserialize_with = "nullable")]
        image: String,
        #[serde(default, deserialize_with = "nullable")]
        command: String,
        #[serde(default, deserialize_with = "nullable")]
        state: String,
        #[serde(default, deserialize_with = "nullable")]
        status: String,
        #[serde(default, deserialize_with = "nullable")]
        ports: String,
        #[serde(default, deserialize_with = "nullable")]
        labels: String,
    }
    let text = gocompat::decode_bytes(output);
    let mut containers = Vec::new();
    for line in text.lines() {
        let item: Item = serde_json::from_str(line)?;
        let (compose_project, compose_service) = compose_labels(&item.labels);
        containers.push(Container {
            id: item.id,
            name: item.names,
            image: item.image,
            state: item.state,
            status: item.status,
            command: item.command,
            ports: item.ports,
            compose_project,
            compose_service,
        });
    }
    gocompat::stable_sort_by(
        &mut containers,
        |a, b| {
            if a.state == b.state { a.name < b.name } else { a.state == "running" }
        },
    );
    Ok(containers)
}

pub fn compose_labels(labels: &str) -> (String, String) {
    let (mut project, mut service) = (String::new(), String::new());
    for label in labels.split(',') {
        match label.split_once('=') {
            Some(("com.docker.compose.project", value)) => project = value.to_string(),
            Some(("com.docker.compose.service", value)) => service = value.to_string(),
            _ => {}
        }
    }
    (project, service)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn docker_context_names() {
        assert_eq!(docker_context("default"), "colima");
        assert_eq!(docker_context("dev"), "colima-dev");
    }

    #[test]
    fn compose_label_parsing() {
        let labels = "com.docker.compose.project=ides,com.docker.compose.service=postgres,other=value";
        assert_eq!(compose_labels(labels), ("ides".to_string(), "postgres".to_string()));
    }

    fn counting_hosts(status: fn(&str) -> Result<Vec<u8>, Error>) -> (DockerHosts, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let hosts = DockerHosts::new(Box::new(move |profile| {
            counter.fetch_add(1, Ordering::SeqCst);
            status(profile)
        }));
        (hosts, calls)
    }

    #[test]
    fn docker_env_uses_colima_socket() {
        let (hosts, calls) = counting_hosts(|name| {
            Ok(format!(r#"{{"display_name":"colima","docker_socket":"unix:///home/u/.colima/{name}/docker.sock"}}"#)
                .into_bytes())
        });
        for _ in 0..2 {
            assert_eq!(hosts.env("dev"), ("DOCKER_HOST", "unix:///home/u/.colima/dev/docker.sock".to_string()));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "socket should be cached");
    }

    #[test]
    fn docker_env_falls_back_to_context() {
        type Status = fn(&str) -> Result<Vec<u8>, Error>;
        let cases: [Status; 3] = [
            |_| Err(Error::invalid("colima is not running")),
            |_| Ok(br#"{"runtime":"containerd"}"#.to_vec()),
            |_| Ok(b"nope".to_vec()),
        ];
        for status in cases {
            let (hosts, calls) = counting_hosts(status);
            for _ in 0..2 {
                assert_eq!(hosts.env("dev"), ("DOCKER_CONTEXT", "colima-dev".to_string()));
            }
            assert_eq!(calls.load(Ordering::SeqCst), 2, "failures must not be cached");
        }
    }

    #[test]
    fn parses_profile_listings() {
        let one = br#"{"name":"default","status":"Stopped","arch":"aarch64","cpus":2,"memory":4294967296,"disk":107374182400,"runtime":"docker"}"#;
        let profiles = parse_profiles(one).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!((profiles[0].cpus, profiles[0].memory), (2, 4294967296));
        let lines = b"{\"name\":\"work\",\"status\":\"Running\"}\n{\"name\":\"default\",\"status\":null}\n";
        let names: Vec<String> = parse_profiles(lines).unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["default", "work"]);
        let array = br#"[{"name":"b"},{"name":"a"}]"#;
        assert_eq!(parse_profiles(array).unwrap()[0].name, "a");
        assert!(parse_profiles(b"{bad").is_err());
    }

    #[test]
    fn parses_and_orders_containers() {
        let output = concat!(
            r#"{"ID":"1","Names":"web","State":"exited","Labels":"com.docker.compose.project=shop"}"#,
            "\n",
            r#"{"ID":"2","Names":"api","State":"running","Labels":""}"#,
            "\n"
        );
        let containers = parse_containers(output.as_bytes()).unwrap();
        assert_eq!(containers[0].id, "2");
        assert_eq!(containers[1].compose_project, "shop");
        assert!(parse_containers(b"not json").is_err());
    }

    #[test]
    fn system_profile_and_containers() {
        if Command::new("colima").arg("version").output().is_err() {
            return;
        }
        let profiles = ExecBackend.profiles().expect("colima list");
        assert!(!profiles.is_empty(), "expected at least one colima profile");
        if Command::new("docker").arg("--version").output().is_err() {
            return;
        }
        if let Err(err) = ExecBackend.containers(&profiles[0].name) {
            assert!(matches!(err, Error::Exit { .. }), "unexpected error: {err}");
        }
    }
}
