//! Container log streaming, retention limits, search and scrolling.

use std::io::{self, PipeReader, Read};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};

use crate::ansi;
use crate::backend::LogRequest;
use crate::error::Error;
use crate::gocompat;
use crate::model::{Cmd, Model, Msg};
use crate::process::{self, Supervised};
use crate::tea::{Key, KeyCode};
use crate::view::sanitize_text;

pub const MAX_LOG_LINES: usize = 10_000;
pub const MAX_LOG_BYTES: usize = 8 << 20;
pub const MAX_LOG_PARTIAL_BYTES: usize = 1 << 20;
const READ_CHUNK: usize = 4096;
const TRUNCATED_MARKER: &str = "[log line truncated] ";

/// A running `docker logs` process with stdout and stderr merged into one pipe.
pub struct LogReader {
    pipe: Mutex<Option<PipeReader>>,
    process: Option<Supervised>,
}

impl LogReader {
    /// A reader with no process behind it; its stream ends immediately.
    #[cfg(test)]
    pub fn detached() -> Arc<Self> {
        Arc::new(Self { pipe: Mutex::new(None), process: None })
    }

    pub fn cancel(&self) {
        if let Some(process) = &self.process {
            process.kill();
        }
    }

    fn exit_error(&self) -> Option<String> {
        match self.process.as_ref()?.wait() {
            Ok(status) if status.success() => None,
            Ok(status) => Some(gocompat::exit_status_text(status)),
            Err(err) => Some(err),
        }
    }

    fn read_chunk(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut pipe = self.pipe.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(pipe) = pipe.as_mut() else {
            return Ok(0);
        };
        loop {
            match pipe.read(buf) {
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
}

pub fn start_log_reader(mut cmd: Command) -> Result<Arc<LogReader>, Error> {
    let pipe = process::merge_output(&mut cmd)?;
    let child = process::spawn(&mut cmd)?;
    drop(cmd);
    Ok(Arc::new(LogReader { pipe: Mutex::new(Some(pipe)), process: Some(process::supervise(child)) }))
}

pub struct LogsMsg {
    pub reader: Arc<LogReader>,
    pub data: Vec<u8>,
    pub done: bool,
    pub err: Option<String>,
}

/// Reads the next chunk; at end of stream it waits for the process so a
/// failed `docker logs` reports its exit status.
pub fn read_logs(reader: Arc<LogReader>) -> LogsMsg {
    let mut buf = vec![0; READ_CHUNK];
    match reader.read_chunk(&mut buf) {
        Ok(n) if n > 0 => {
            buf.truncate(n);
            LogsMsg { reader, data: buf, done: false, err: None }
        }
        Ok(_) => {
            let err = reader.exit_error();
            LogsMsg { reader, data: Vec::new(), done: true, err }
        }
        Err(err) => LogsMsg { reader, data: Vec::new(), done: true, err: Some(err.to_string()) },
    }
}

impl Model {
    #[cfg(test)]
    pub fn visible_logs(&self, count: usize) -> Vec<&str> {
        let (start, end) = self.visible_log_range(count);
        self.filtered_logs()[start..end].to_vec()
    }

    /// The window of filtered log indices shown for the current scroll position.
    pub fn visible_log_range(&self, count: usize) -> (usize, usize) {
        let filtered = self.filtered_logs().len();
        if count == 0 || filtered == 0 {
            return (0, 0);
        }
        if self.log_scroll >= filtered {
            return (0, count.min(filtered));
        }
        let end = filtered - self.log_scroll;
        (end.saturating_sub(count), end)
    }

    /// Turns logical log entries into bounded terminal rows plus the filtered
    /// log index behind each row. Wrapping here, rather than in the terminal,
    /// keeps the pane height stable; the indices map mouse rows back to lines.
    pub fn log_rows_indexed(&self, count: usize, width: usize) -> (Vec<String>, Vec<usize>) {
        if count == 0 {
            return (Vec::new(), Vec::new());
        }
        let (start, end) = self.visible_log_range(count);
        let filtered = self.filtered_logs();
        let (mut rows, mut indices) = (Vec::new(), Vec::new());
        for (i, line) in filtered.iter().enumerate().take(end).skip(start) {
            let text = self.log_text(line);
            if self.log_wrap {
                for row in ansi::hardwrap(&text, width).split('\n') {
                    rows.push(row.to_string());
                    indices.push(i);
                }
            } else {
                rows.push(ansi::truncate(&text, width, ""));
                indices.push(i);
            }
        }
        if rows.len() > count {
            let excess = rows.len() - count;
            rows.drain(..excess);
            indices.drain(..excess);
        }
        (rows, indices)
    }

    pub fn append_logs(&mut self, data: &[u8]) {
        let mut buffer = std::mem::take(&mut self.log_partial);
        buffer.extend_from_slice(data);
        let mut start = 0;
        while let Some(end) = memchr::memchr(b'\n', &buffer[start..]) {
            self.append_raw_line(&buffer[start..start + end]);
            start += end + 1;
        }
        buffer.drain(..start);
        self.log_partial = buffer;
        if self.log_partial.len() > MAX_LOG_PARTIAL_BYTES {
            self.log_partial.drain(..self.log_partial.len() - MAX_LOG_PARTIAL_BYTES);
            self.logs_truncated = true;
            self.partial_trimmed = true;
        }
        if self.log_scroll == 0 {
            return;
        }
        self.log_scroll = self.log_scroll.min(self.logs.len().saturating_sub(1));
    }

    pub fn finish_logs(&mut self) {
        if !self.log_partial.is_empty() {
            let partial = std::mem::take(&mut self.log_partial);
            self.append_raw_line(&partial);
        }
    }

    fn append_raw_line(&mut self, raw: &[u8]) {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        let mut line = gocompat::decode_bytes(raw);
        if self.partial_trimmed {
            line.insert_str(0, TRUNCATED_MARKER);
            self.partial_trimmed = false;
        }
        self.append_log_line(line);
    }

    pub fn append_log_line(&mut self, line: String) {
        self.log_bytes += line.len();
        self.logs.push_back(line);
        while self.logs.len() > MAX_LOG_LINES || self.log_bytes > MAX_LOG_BYTES {
            let Some(dropped) = self.logs.pop_front() else {
                break;
            };
            self.log_bytes -= dropped.len();
            self.logs_truncated = true;
            // Trimming shifts line indices, so any selection no longer matches.
            self.clear_log_selection();
        }
    }

    pub fn clear_log_selection(&mut self) {
        self.log_selecting = false;
        self.log_sel_active = false;
        self.log_sel_dragged = false;
        self.log_sel_start = 0;
        self.log_sel_end = 0;
    }

    pub fn reset_logs(&mut self) {
        self.logs.clear();
        self.log_partial.clear();
        self.log_scroll = 0;
        self.log_from_start = false;
        self.log_bytes = 0;
        self.logs_truncated = false;
        self.partial_trimmed = false;
        self.clear_log_selection();
    }

    pub fn scroll_logs(&mut self, key: &str) {
        match key {
            "pgup" => self.log_scroll = self.filtered_logs().len().min(self.log_scroll + 10),
            "pgdown" => self.log_scroll = self.log_scroll.saturating_sub(10),
            "home" => self.log_scroll = self.logs.len(),
            "end" => self.log_scroll = 0,
            _ => {}
        }
    }

    pub fn reload_selected_logs(&mut self, from_start: bool) -> Option<Cmd> {
        self.stop_logs();
        self.reset_logs();
        self.log_from_start = from_start;
        self.err = None;
        if self.status == "logs failed" {
            self.status = "ready".to_string();
        }
        // A group header has no stream; follow stays armed so it resumes on
        // the next container selection.
        let id = self.selected_id();
        if id.is_empty() {
            return None;
        }
        let req = LogRequest { follow: self.follow, from_start, since: String::new() };
        match self.backend.open_logs(&self.current_profile_name(), &id, &req) {
            Ok(reader) => {
                self.reader = Some(reader);
                self.read_logs_cmd()
            }
            Err(err) => {
                self.err = Some(err.to_string());
                self.status = "logs failed".to_string();
                None
            }
        }
    }

    /// Reopens a follow stream after `docker logs` exits (the container
    /// stopped or restarted), resuming just past the newest retained
    /// timestamp so nothing is duplicated and the buffer is kept.
    pub fn resume_follow_logs(&mut self) -> Option<Cmd> {
        let req = LogRequest { follow: true, from_start: false, since: self.last_log_since() };
        match self.backend.open_logs(&self.current_profile_name(), &self.selected_id(), &req) {
            Ok(reader) => {
                self.reader = Some(reader);
                self.read_logs_cmd()
            }
            Err(err) => {
                self.follow = false;
                self.err = Some(err.to_string());
                self.status = "logs failed".to_string();
                None
            }
        }
    }

    /// An RFC3339Nano instant just after the newest retained log timestamp,
    /// suitable for `docker logs --since`.
    pub fn last_log_since(&self) -> String {
        self.logs
            .iter()
            .rev()
            .filter_map(|line| line.split_once(' '))
            .find_map(|(stamp, _)| gocompat::parse_rfc3339(stamp))
            .map(|t| gocompat::format_rfc3339_nano(&(t + chrono::Duration::nanoseconds(1))))
            .unwrap_or_default()
    }

    pub fn stop_logs(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.cancel();
        }
    }

    pub fn read_logs_cmd(&self) -> Option<Cmd> {
        let reader = Arc::clone(self.reader.as_ref()?);
        Some(Cmd::run(move || Msg::Logs(read_logs(reader))))
    }

    pub fn apply_logs(&mut self, msg: LogsMsg) -> Option<Cmd> {
        if !self.reader.as_ref().is_some_and(|r| Arc::ptr_eq(r, &msg.reader)) {
            return None;
        }
        if !msg.data.is_empty() {
            self.append_logs(&msg.data);
        }
        if let Some(err) = &msg.err {
            self.err = Some(err.clone());
            self.status = "logs failed".to_string();
        }
        if !msg.done {
            return self.read_logs_cmd();
        }
        self.reader = None;
        let mut partial = gocompat::decode_bytes(&self.log_partial).trim().to_string();
        self.finish_logs();
        if let Some(mut err) = msg.err {
            if partial.is_empty() {
                partial = self.logs.back().map(|l| l.trim().to_string()).unwrap_or_default();
            }
            if !partial.is_empty() {
                err = format!("{err}: {partial}");
            }
            self.err = Some(err);
            self.status = "logs failed".to_string();
            self.follow = false;
        }
        if self.log_from_start {
            self.log_scroll = self.logs.len();
            self.log_from_start = false;
        }
        // docker logs --follow exits when the container stops; keep following
        // and reconnect so a restart resumes the stream.
        if self.follow { Some(crate::update::log_retry_tick()) } else { None }
    }

    /// Docker timestamps are always captured; toggling only changes presentation.
    pub fn log_text(&self, line: &str) -> String {
        let mut text = line;
        if !self.log_timestamps
            && let Some((stamp, rest)) = line.split_once(' ')
            && gocompat::parse_rfc3339(stamp).is_some()
        {
            text = rest;
        }
        sanitize_text(text)
    }

    pub fn filtered_logs(&self) -> Vec<&str> {
        let query = self.log_query.trim().to_lowercase();
        self.logs
            .iter()
            .filter(|line| query.is_empty() || sanitize_text(line).to_lowercase().contains(&query))
            .map(String::as_str)
            .collect()
    }

    pub fn log_search_key(&mut self, msg: &Key) -> Option<Cmd> {
        match &msg.code {
            KeyCode::Enter => self.log_search_editing = false,
            KeyCode::Esc => {
                self.log_query = self.log_search_before.clone();
                self.log_search_editing = false;
            }
            KeyCode::Ctrl('u') => self.log_query.clear(),
            KeyCode::Backspace => {
                self.log_query.pop();
            }
            KeyCode::Space => {
                if self.log_query.chars().count() < 256 {
                    self.log_query.push(' ');
                }
            }
            KeyCode::Runes(text) if self.log_query.chars().count() + text.chars().count() <= 256 => {
                self.log_query.push_str(text);
            }
            _ => {}
        }
        self.log_scroll = 0;
        // The query changes which lines the selection indices point at.
        self.clear_log_selection();
        None
    }

    pub fn pause_logs(&mut self) {
        if self.follow {
            self.stop_logs();
            self.finish_logs();
            self.follow = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Msg;
    use crate::tea::{Key, KeyCode};
    use crate::testutil::{FakeBackend, container, model, model_with, profile};
    use crate::update::shortcut_key;

    fn drain(reader: Arc<LogReader>) -> (String, Option<String>) {
        let mut output = Vec::new();
        loop {
            let msg = read_logs(Arc::clone(&reader));
            output.extend_from_slice(&msg.data);
            if msg.done {
                return (String::from_utf8_lossy(&output).into_owned(), msg.err);
            }
        }
    }

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn read_logs_merges_output_and_reports_failure() {
        let reader = start_log_reader(sh("printf stdout; printf stderr >&2; exit 7")).unwrap();
        let (output, err) = drain(reader);
        assert!(output.contains("stdout") && output.contains("stderr"), "merged output = {output:?}");
        assert_eq!(err.as_deref(), Some("exit status 7"));
    }

    #[test]
    fn read_all_log_reader() {
        let (output, err) = drain(start_log_reader(sh("printf partial")).unwrap());
        assert_eq!((output.as_str(), err), ("partial", None));
    }

    #[test]
    fn cancel_ends_a_follow_stream() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let reader = start_log_reader(cmd).unwrap();
        reader.cancel();
        let (_, err) = drain(reader);
        assert_eq!(err.as_deref(), Some("signal: killed"));
    }

    #[test]
    fn log_buffers_are_bounded() {
        let mut m = model();
        let chunk = vec![b'x'; 4096];
        for _ in 0..512 {
            m.append_logs(&chunk);
        }
        assert!(m.log_partial.len() <= MAX_LOG_PARTIAL_BYTES);
    }

    #[test]
    fn finish_logs_keeps_partial_line() {
        let mut m = model();
        m.append_logs(b"complete\npartial");
        m.finish_logs();
        assert_eq!(m.logs, ["complete", "partial"]);
        assert!(m.log_partial.is_empty());
    }

    #[test]
    fn multibyte_characters_split_across_chunks_survive() {
        let mut m = model();
        let bytes = "数据\n".as_bytes();
        m.append_logs(&bytes[..2]);
        m.append_logs(&bytes[2..]);
        assert_eq!(m.logs, ["数据"]);
    }

    #[test]
    fn visible_logs_at_start() {
        let mut m = model();
        m.logs = ["first", "second", "third", "last"].map(String::from).into();
        m.log_scroll = 4;
        assert_eq!(m.visible_logs(2), ["first", "second"]);
    }

    #[test]
    fn log_search_and_timestamps() {
        let mut m = model();
        m.logs = ["2026-09-05T12:00:00.123Z ERROR failed", "plain message", "2026-09-05T12:01:00Z error again"]
            .map(String::from)
            .into();
        m.log_query = "ERROR".into();
        let got: Vec<String> = m.visible_logs(10).into_iter().map(String::from).collect();
        assert_eq!(got.len(), 2, "matches: {got:?}");
        assert_eq!(m.log_text(&got[0]), "ERROR failed", "timestamp not hidden");
        m.log_timestamps = true;
        assert_eq!(m.log_text(&got[0]), got[0], "timestamp not shown");
        assert_eq!(m.log_text("plain message"), "plain message");
        m.log_query = "absent".into();
        assert!(m.visible_logs(10).is_empty(), "nonmatching lines displayed");
    }

    #[test]
    fn log_pause_preserves_buffer() {
        let mut m = model();
        m.logs = ["one", "two"].map(String::from).into();
        m.follow = true;
        assert!(m.key(shortcut_key("f")).is_none());
        assert!(!m.follow && m.logs.len() == 2, "pause lost logs");
        m.follow = true;
        m.key(Key::new(KeyCode::PgUp));
        assert!(!m.follow && m.log_scroll == 2, "scroll did not pause");
        m.key(shortcut_key("L"));
        m.key(shortcut_key("数据库"));
        m.key(Key::new(KeyCode::Backspace));
        assert_eq!(m.log_query, "数据", "unicode editing failed");
        m.key(Key::new(KeyCode::Esc));
        assert!(!m.log_search_editing && m.log_query.is_empty(), "cancel failed");
    }

    #[test]
    fn log_memory_notice_and_partial_attribution() {
        let mut m = model();
        m.containers = vec![container("a", "", "")];
        let mut data = b"good\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', MAX_LOG_PARTIAL_BYTES + 20));
        m.append_logs(&data);
        assert_eq!(m.logs[0], "good", "truncation attributed to wrong line");
        assert!(m.partial_trimmed && m.log_partial.len() <= MAX_LOG_PARTIAL_BYTES, "partial not bounded");
        assert!(m.render_logs(8, 80).contains("logs truncated"), "missing immediate notice");
        m.finish_logs();
        assert!(m.logs[1].starts_with(TRUNCATED_MARKER), "missing line marker");
        for _ in 0..=MAX_LOG_LINES {
            m.append_log_line("line".into());
        }
        assert!(m.logs.len() <= MAX_LOG_LINES && m.log_bytes <= MAX_LOG_BYTES && m.logs_truncated);
        m.append_log_line("x".repeat(MAX_LOG_BYTES + 1));
        assert!(m.log_bytes <= MAX_LOG_BYTES, "byte limit failed");
    }

    #[test]
    fn finished_log_error_includes_output() {
        let mut m = model();
        let reader = LogReader::detached();
        m.reader = Some(Arc::clone(&reader));
        m.update(Msg::Logs(LogsMsg {
            reader,
            data: b"docker error\n".to_vec(),
            done: true,
            err: Some("exit status 1".into()),
        }));
        assert!(m.err.as_deref().is_some_and(|e| e.contains("docker error")), "error = {:?}", m.err);
    }

    #[test]
    fn selecting_a_compose_group_keeps_follow_armed() {
        let mut m = model();
        m.follow = true;
        m.containers = vec![crate::model::Container {
            compose_project: "ides".into(),
            ..container("postgres", "postgres", "running")
        }];
        m.expanded.insert("ides".into(), true);
        assert!(m.reload_selected_logs(false).is_none() && m.follow && m.reader.is_none());
    }

    fn follow_model(backend: &Arc<FakeBackend>, state: &str) -> Model {
        let mut m = model_with(backend);
        m.profiles = vec![profile("default", "Running")];
        m.containers = vec![container("one", "one", state)];
        m.follow = true;
        m
    }

    #[test]
    fn follow_reconnects_after_stream_ends() {
        let backend = FakeBackend::new();
        let mut m = follow_model(&backend, "running");
        let reader = LogReader::detached();
        m.reader = Some(Arc::clone(&reader));
        m.logs = ["2024-01-02T03:04:05.000000001Z hello".to_string()].into();
        let cmd = m.update(Msg::Logs(LogsMsg { reader, data: Vec::new(), done: true, err: None }));
        assert!(
            m.follow && m.reader.is_none() && cmd.is_some(),
            "stream end = follow {} retry {}",
            m.follow,
            cmd.is_some()
        );
        m.update(Msg::LogRetry);
        let state = backend.state();
        assert!(m.follow && state.log_request.follow);
        assert_eq!(state.log_request.since, "2024-01-02T03:04:05.000000002Z");
        assert_eq!(m.logs.len(), 1, "reconnect cleared retained logs");
    }

    #[test]
    fn follow_survives_navigating_through_group_header() {
        let backend = FakeBackend::new();
        let mut m = model_with(&backend);
        m.profiles = vec![profile("default", "Running")];
        m.containers = vec![
            crate::model::Container {
                compose_project: "app".into(),
                compose_service: "a".into(),
                ..container("a", "app-a", "running")
            },
            crate::model::Container {
                compose_project: "app".into(),
                compose_service: "b".into(),
                ..container("b", "app-b", "running")
            },
        ];
        m.follow = true;
        m.container_index = 1;
        m.key(Key::new(KeyCode::Up));
        assert!(m.follow && m.selected_id().is_empty());
        m.key(Key::new(KeyCode::Down));
        let state = backend.state();
        assert!(m.follow && state.log_id == "a" && state.log_request.follow);
    }

    #[test]
    fn follow_survives_refresh_selection_change() {
        let backend = FakeBackend::new();
        let mut m = follow_model(&backend, "running");
        m.width = 100;
        m.containers = vec![container("old", "web", "running")];
        m.applied_refresh_id = 1;
        m.refresh_id = 1;
        let profiles = m.profiles.clone();
        m.update(Msg::Refresh(crate::model::RefreshMsg {
            profile_name: "default".into(),
            request_id: 2,
            profiles,
            containers: vec![container("new", "web", "running")],
            err: None,
            list_failed: false,
        }));
        let state = backend.state();
        assert!(m.follow && state.log_id == "new" && state.log_request.follow);
    }

    #[test]
    fn follow_retry_waits_for_stopped_container() {
        let backend = FakeBackend::new();
        let mut m = follow_model(&backend, "exited");
        let cmd = m.update(Msg::LogRetry);
        assert!(m.follow && cmd.is_some() && backend.state().log_id.is_empty());
    }

    #[test]
    fn follow_stops_on_stream_error_or_pause() {
        let backend = FakeBackend::new();
        let mut m = follow_model(&backend, "running");
        let reader = LogReader::detached();
        m.reader = Some(Arc::clone(&reader));
        let cmd = m.update(Msg::Logs(LogsMsg { reader, data: Vec::new(), done: true, err: Some("boom".into()) }));
        assert!(!m.follow && cmd.is_none());
        m.follow = false;
        assert!(m.update(Msg::LogRetry).is_none(), "paused follow still scheduled a retry");
    }

    #[test]
    fn non_follow_stream_end_does_not_retry() {
        let backend = FakeBackend::new();
        let mut m = follow_model(&backend, "running");
        m.follow = false;
        let reader = LogReader::detached();
        m.reader = Some(Arc::clone(&reader));
        assert!(m.update(Msg::Logs(LogsMsg { reader, data: Vec::new(), done: true, err: None })).is_none());
    }
}
