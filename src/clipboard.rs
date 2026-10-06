//! Copying container details and log text to the system clipboard.

use std::io::{self, Write};
use std::process::{Command, Stdio};

use base64::Engine;

use crate::error::Error;
use crate::model::{Cmd, Container, Model};

pub fn copy_to_clipboard(text: &str) -> Result<(), Error> {
    let mut cmd = Command::new("pbcopy");
    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(text.as_bytes())?;
            }
            let status = child.wait()?;
            if status.success() { Ok(()) } else { Err(Error::Exit { status, output: String::new() }) }
        }
        // OSC52 fallback for hosts without pbcopy; stderr shares the tty but
        // bypasses the renderer's stdout writes.
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let payload = base64::engine::general_purpose::STANDARD.encode(text);
            let mut stderr = io::stderr();
            stderr.write_all(format!("\x1b]52;c;{payload}\x07").as_bytes())?;
            Ok(stderr.flush()?)
        }
        Err(err) => Err(Error::spawn("pbcopy", err)),
    }
}

impl Model {
    pub fn copy_text(&mut self, text: &str, label: &str) -> Option<Cmd> {
        match (self.clipboard)(text) {
            Ok(()) => {
                self.err = None;
                self.status = format!("copied {label}");
            }
            Err(err) => {
                self.err = Some(err.to_string());
                self.status = "copy failed".to_string();
            }
        }
        None
    }

    pub fn copy_selected_details(&mut self) -> Option<Cmd> {
        let Some(c) = self.selected_container().cloned() else {
            self.status = "select a container to copy its details".to_string();
            return None;
        };
        self.copy_text(&container_details_text(&c), &format!("details for {}", c.list_name()))
    }

    pub fn copy_filtered_logs(&mut self) -> Option<Cmd> {
        let lines = self.log_text_lines(&self.filtered_logs());
        if lines.is_empty() {
            self.status = "no logs to copy".to_string();
            return None;
        }
        self.copy_text(&lines.join("\n"), &count_label(lines.len(), "log line"))
    }

    pub fn log_text_lines(&self, lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| self.log_text(line)).collect()
    }
}

pub fn container_details_text(c: &Container) -> String {
    [
        ("name", &c.name),
        ("state", &c.state),
        ("status", &c.status),
        ("image", &c.image),
        ("id", &c.id),
        ("command", &c.command),
        ("ports", &c.ports),
        ("compose project", &c.compose_project),
        ("compose service", &c.compose_service),
    ]
    .iter()
    .filter(|(_, value)| !value.is_empty())
    .map(|(key, value)| format!("{key}: {value}"))
    .collect::<Vec<_>>()
    .join("\n")
}

pub fn count_label(count: usize, noun: &str) -> String {
    if count == 1 { format!("1 {noun}") } else { format!("{count} {noun}s") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tea::Key;
    use crate::testutil::{container, model, profile, stub_clipboard};

    #[test]
    fn copy_container_details_key() {
        let (clipboard, copied) = stub_clipboard();
        let mut m = model();
        m.clipboard = clipboard;
        m.profiles = vec![profile("default", "Running")];
        m.containers = vec![Container {
            image: "nginx".into(),
            status: "Up 2 hours".into(),
            ports: "80/tcp".into(),
            ..container("abc123", "web", "running")
        }];
        m.key(Key::runes("y"));
        let copied = copied.lock().unwrap().clone();
        for want in ["name: web", "id: abc123", "image: nginx", "ports: 80/tcp"] {
            assert!(copied.contains(want), "details copy missing {want:?} in {copied:?}");
        }
        assert!(!copied.contains("compose") && m.status == "copied details for web");
    }

    #[test]
    fn copy_all_logs_key_strips_timestamps() {
        let (clipboard, copied) = stub_clipboard();
        let mut m = model();
        m.clipboard = clipboard;
        m.logs = ["2024-01-01T00:00:00Z alpha", "2024-01-01T00:00:01Z beta"].map(String::from).into();
        m.key(Key::runes("Y"));
        assert_eq!(*copied.lock().unwrap(), "alpha\nbeta");
        assert_eq!(m.status, "copied 2 log lines");
        m.logs.clear();
        m.key(Key::runes("Y"));
        assert_eq!(m.status, "no logs to copy");
    }
}
