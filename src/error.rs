//! The crate's error type; messages keep the Go implementation's wording.

use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use crate::gocompat;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("exec: {program:?}: executable file not found in $PATH")]
    NotFound { program: String },
    #[error("{}", exit_text(.status, .output))]
    Exit { status: ExitStatus, output: String },
    #[error("{command} timed out after {}", gocompat::format_duration(*.after))]
    Timeout { command: String, after: Duration },
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Http(#[from] ureq::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("the menu bar item is already running")]
    MenubarRunning,
    #[error("{context}: {source}")]
    Context { context: String, source: Box<Error> },
}

impl Error {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub fn context(self, context: impl Into<String>) -> Self {
        Self::Context { context: context.into(), source: Box::new(self) }
    }

    /// Classifies a failed spawn; a missing binary reads like Go's exec error.
    pub fn spawn(program: &str, err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::NotFound {
            Self::NotFound { program: program.to_string() }
        } else {
            Self::Io(err)
        }
    }

    pub fn is_permission_denied(&self) -> bool {
        match self {
            Self::Io(err) => err.kind() == io::ErrorKind::PermissionDenied,
            Self::Context { source, .. } => source.is_permission_denied(),
            _ => false,
        }
    }
}

fn exit_text(status: &ExitStatus, output: &str) -> String {
    let status = gocompat::exit_status_text(*status);
    if output.is_empty() { status } else { format!("{status}: {output}") }
}
