//! Application state and the messages that drive it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::autostop::AUTO_STOP_DEFAULT;
use crate::backend::{Backend, nullable};
use crate::clipboard;
use crate::error::Error;
use crate::logs::LogsMsg;
use crate::menubar_proc;
use crate::overview::{CleanupMsg, StorageSample};
use crate::stats::StatsSample;
use crate::tea::{self, Event, Key, Mouse};

pub type Cmd = tea::Cmd<Msg>;
pub type TickFactory = Arc<dyn Fn() -> Option<Cmd> + Send + Sync>;
pub type Clipboard = Arc<dyn Fn(&str) -> Result<(), Error> + Send + Sync>;

pub const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Profile {
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub status: String,
    #[serde(default, deserialize_with = "nullable")]
    pub arch: String,
    #[serde(default, deserialize_with = "nullable")]
    pub cpus: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub memory: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub disk: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub runtime: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
    pub command: String,
    pub ports: String,
    pub compose_project: String,
    pub compose_service: String,
}

impl Container {
    pub fn list_name(&self) -> &str {
        if self.compose_service.is_empty() { &self.name } else { &self.compose_service }
    }
}

pub struct RefreshMsg {
    pub profile_name: String,
    pub request_id: u64,
    pub profiles: Vec<Profile>,
    pub containers: Vec<Container>,
    pub err: Option<String>,
    pub list_failed: bool,
}

pub struct ActionMsg {
    pub request_id: u64,
    pub err: Option<String>,
}

pub enum Msg {
    Key(Key),
    Mouse(Mouse),
    WindowSize { width: usize, height: usize },
    Refresh(RefreshMsg),
    Action(ActionMsg),
    Tick,
    SpinnerTick,
    LogRetry,
    ExecDone(Result<ExitStatus, Error>),
    Logs(LogsMsg),
    UpdateCheck(String),
    StatsTick,
    Stats(StatsSample),
    Storage(StorageSample),
    Cleanup(CleanupMsg),
}

impl From<Event> for Msg {
    fn from(event: Event) -> Self {
        match event {
            Event::Key(key) => Self::Key(key),
            Event::Mouse(mouse) => Self::Mouse(mouse),
            Event::Resize { width, height } => Self::WindowSize { width, height },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveAction {
    pub container_id: String,
    pub label: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Containers,
    Logs,
}

impl Focus {
    pub fn toggled(self) -> Self {
        match self {
            Self::Containers => Self::Logs,
            Self::Logs => Self::Containers,
        }
    }
}

#[derive(Clone)]
pub struct Model {
    pub usage_overview: bool,
    pub confirm_cleanup: bool,
    pub cleanup_choice: usize,
    pub cleanup_running: bool,
    pub overall: Option<StatsSample>,
    pub storage: Option<StorageSample>,
    pub storage_busy: bool,
    pub storage_requested: Option<Instant>,
    pub stats_busy: bool,
    pub log_query: String,
    pub log_search_editing: bool,
    pub log_search_before: String,
    pub log_timestamps: bool,
    pub log_wrap: bool,
    pub menubar: bool,
    pub search_query: String,
    pub search_editing: bool,
    pub search_before: String,
    pub running_only: bool,
    pub profiles: Vec<Profile>,
    pub profile_index: usize,
    pub containers: Vec<Container>,
    pub container_index: usize,
    pub focus: Focus,
    pub width: usize,
    pub height: usize,
    pub status: String,
    pub err: Option<String>,
    pub confirm_delete: bool,
    pub delete_profile: String,
    pub delete_id: String,
    pub delete_choice: usize,
    pub logs: VecDeque<String>,
    pub log_partial: Vec<u8>,
    pub log_bytes: usize,
    pub logs_truncated: bool,
    pub partial_trimmed: bool,
    pub log_scroll: usize,
    pub log_from_start: bool,
    pub log_selecting: bool,
    pub log_sel_dragged: bool,
    pub log_sel_active: bool,
    pub log_sel_start: usize,
    pub log_sel_end: usize,
    pub follow: bool,
    pub reader: Option<Arc<crate::logs::LogReader>>,
    pub expanded: HashMap<String, bool>,
    pub refresh_id: u64,
    pub applied_refresh_id: u64,
    pub next_action_id: u64,
    pub active_actions: BTreeMap<u64, ActiveAction>,
    pub spinner_frame: usize,
    pub update_version: String,
    pub action_menu: bool,
    pub action_index: usize,
    pub auto_stop: bool,
    pub auto_stop_after: Duration,
    pub auto_stop_pinned: bool,
    pub idle_profile: String,
    pub idle_since: Option<Instant>,
    pub settings_file: Option<PathBuf>,
    pub backend: Arc<dyn Backend>,
    pub tick: TickFactory,
    pub now: Arc<dyn Fn() -> Instant + Send + Sync>,
    pub menubar_alive: Arc<dyn Fn() -> bool + Send + Sync>,
    pub clipboard: Clipboard,
}

impl Model {
    pub fn new(backend: Arc<dyn Backend>, tick: Option<TickFactory>) -> Self {
        Self {
            usage_overview: false,
            confirm_cleanup: false,
            cleanup_choice: 0,
            cleanup_running: false,
            overall: None,
            storage: None,
            storage_busy: false,
            storage_requested: None,
            stats_busy: false,
            log_query: String::new(),
            log_search_editing: false,
            log_search_before: String::new(),
            log_timestamps: false,
            log_wrap: false,
            menubar: false,
            search_query: String::new(),
            search_editing: false,
            search_before: String::new(),
            running_only: false,
            profiles: Vec::new(),
            profile_index: 0,
            containers: Vec::new(),
            container_index: 0,
            focus: Focus::Containers,
            width: 0,
            height: 0,
            status: "loading".to_string(),
            err: None,
            confirm_delete: false,
            delete_profile: String::new(),
            delete_id: String::new(),
            delete_choice: 0,
            logs: VecDeque::new(),
            log_partial: Vec::new(),
            log_bytes: 0,
            logs_truncated: false,
            partial_trimmed: false,
            log_scroll: 0,
            log_from_start: false,
            log_selecting: false,
            log_sel_dragged: false,
            log_sel_active: false,
            log_sel_start: 0,
            log_sel_end: 0,
            follow: false,
            reader: None,
            expanded: HashMap::new(),
            refresh_id: 1,
            applied_refresh_id: 0,
            next_action_id: 0,
            active_actions: BTreeMap::new(),
            spinner_frame: 0,
            update_version: String::new(),
            action_menu: false,
            action_index: 0,
            auto_stop: false,
            auto_stop_after: AUTO_STOP_DEFAULT,
            auto_stop_pinned: false,
            idle_profile: String::new(),
            idle_since: None,
            settings_file: None,
            backend,
            tick: tick.unwrap_or_else(|| Arc::new(default_tick)),
            now: Arc::new(Instant::now),
            menubar_alive: Arc::new(menubar_proc::menubar_alive),
            clipboard: Arc::new(clipboard::copy_to_clipboard),
        }
    }

    pub fn clock(&self) -> Instant {
        (self.now)()
    }

    pub fn next_tick(&self) -> Option<Cmd> {
        (self.tick)()
    }
}

fn default_tick() -> Option<Cmd> {
    Some(Cmd::Tick(REFRESH_INTERVAL, Msg::Tick))
}

impl tea::App for Model {
    type Msg = Msg;

    fn init(&mut self) -> Option<Cmd> {
        Model::init(self)
    }

    fn update(&mut self, msg: Msg) -> Option<Cmd> {
        Model::update(self, msg)
    }

    fn view(&self) -> String {
        Model::view(self)
    }
}
