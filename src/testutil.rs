//! Test doubles and helpers shared by the unit tests.

use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::backend::{Backend, LogRequest};
use crate::error::Error;
use crate::logs::LogReader;
use crate::model::{Clipboard, Cmd, Container, Model, Msg, Profile, RefreshMsg};
use crate::overview::StorageRow;
use crate::stats::ContainerStats;

#[derive(Default)]
pub struct FakeState {
    pub profiles: Vec<Profile>,
    pub containers: Vec<Container>,
    pub profiles_err: Option<String>,
    pub containers_profile: String,
    pub action_profile: String,
    pub action_command: String,
    pub action_args: Vec<String>,
    pub action_calls: usize,
    pub log_profile: String,
    pub log_id: String,
    pub log_request: LogRequest,
    pub shell_profile: String,
    pub shell_id: String,
    pub cleanups: usize,
    pub cleanup_err: Option<String>,
}

/// Records every call; stats and storage return one fixed sample.
#[derive(Default)]
pub struct FakeBackend {
    state: Mutex<FakeState>,
}

impl FakeBackend {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Backend for FakeBackend {
    fn profiles(&self) -> Result<Vec<Profile>, Error> {
        let state = self.state();
        match &state.profiles_err {
            Some(err) => Err(Error::invalid(err.clone())),
            None => Ok(state.profiles.clone()),
        }
    }

    fn containers(&self, profile: &str) -> Result<Vec<Container>, Error> {
        let mut state = self.state();
        state.containers_profile = profile.to_string();
        Ok(state.containers.clone())
    }

    fn action(&self, profile: &str, command: &str, args: &[String]) -> Result<(), Error> {
        let mut state = self.state();
        state.action_calls += 1;
        state.action_profile = profile.to_string();
        state.action_command = command.to_string();
        state.action_args = args.to_vec();
        Ok(())
    }

    fn open_logs(&self, profile: &str, id: &str, req: &LogRequest) -> Result<Arc<LogReader>, Error> {
        let mut state = self.state();
        state.log_profile = profile.to_string();
        state.log_id = id.to_string();
        state.log_request = req.clone();
        Ok(LogReader::detached())
    }

    fn shell(&self, profile: &str, id: &str) -> Command {
        let mut state = self.state();
        state.shell_profile = profile.to_string();
        state.shell_id = id.to_string();
        Command::new("true")
    }

    fn all_stats(&self, _profile: &str) -> Result<Vec<ContainerStats>, Error> {
        Ok(vec![ContainerStats {
            id: "a".into(),
            cpu: "50%".into(),
            memory: "1MiB / 2GiB".into(),
            network: "0B / 0B".into(),
        }])
    }

    fn storage(&self, _profile: &str) -> Result<Vec<StorageRow>, Error> {
        Ok(vec![StorageRow { r#type: "Images".into(), size: "1GB".into(), reclaimable: "0B".into() }])
    }

    fn cleanup(&self, _profile: &str) -> Result<(), Error> {
        let mut state = self.state();
        state.cleanups += 1;
        match &state.cleanup_err {
            Some(err) => Err(Error::invalid(err.clone())),
            None => Ok(()),
        }
    }
}

/// A model on a fake backend with no timers and no menu bar process.
pub fn model_with(backend: &Arc<FakeBackend>) -> Model {
    let mut m = Model::new(Arc::clone(backend) as Arc<dyn Backend>, Some(Arc::new(|| None)));
    m.menubar_alive = Arc::new(|| false);
    m.clipboard = Arc::new(|_| Err(Error::invalid("clipboard disabled in tests")));
    m
}

pub fn model() -> Model {
    model_with(&FakeBackend::new())
}

pub fn profile(name: &str, status: &str) -> Profile {
    Profile { name: name.into(), status: status.into(), ..Profile::default() }
}

pub fn container(id: &str, name: &str, state: &str) -> Container {
    Container { id: id.into(), name: name.into(), state: state.into(), ..Container::default() }
}

pub fn refresh(profiles: Vec<Profile>, containers: Vec<Container>) -> Msg {
    Msg::Refresh(RefreshMsg {
        profile_name: String::new(),
        request_id: 0,
        profiles,
        containers,
        err: None,
        list_failed: false,
    })
}

pub fn stub_clipboard() -> (Clipboard, Arc<Mutex<String>>) {
    let copied = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&copied);
    let clipboard: Clipboard = Arc::new(move |text: &str| {
        *sink.lock().unwrap() = text.to_string();
        Ok(())
    });
    (clipboard, copied)
}

/// Runs a single worker command synchronously and returns its message.
pub fn run_one(cmd: Cmd) -> Msg {
    match cmd {
        Cmd::Run(f) => f(),
        Cmd::Tick(_, msg) => msg,
        _ => panic!("expected a single message command"),
    }
}

/// Runs every worker command in `cmd`, ticks included without waiting.
pub fn run_all(cmd: Cmd) -> Vec<Msg> {
    match cmd {
        Cmd::Batch(cmds) => cmds.into_iter().flat_map(run_all).collect(),
        Cmd::Run(_) | Cmd::Tick(..) => vec![run_one(cmd)],
        Cmd::Exec(..) | Cmd::Quit => Vec::new(),
    }
}

/// A manually advanced clock for idle timers.
#[derive(Clone)]
pub struct Clock(Arc<Mutex<Instant>>);

impl Clock {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    pub fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }

    pub fn source(&self) -> Arc<dyn Fn() -> Instant + Send + Sync> {
        let clock = Arc::clone(&self.0);
        Arc::new(move || *clock.lock().unwrap())
    }
}
