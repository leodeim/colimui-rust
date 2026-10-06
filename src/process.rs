//! Child process plumbing: bounded queries, merged-output runs, and
//! supervision that lets one thread wait while another kills safely.

use std::io::{self, PipeReader, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::gocompat;

type WaitResult = Result<ExitStatus, String>;

/// A child reaped by a background thread. The reaper only collects the exit
/// status while holding the lock, so `kill` can never signal a reused pid.
pub struct Supervised {
    pid: libc::pid_t,
    state: Arc<(Mutex<Option<WaitResult>>, Condvar)>,
}

pub fn supervise(mut child: Child) -> Supervised {
    let pid = child.id() as libc::pid_t;
    let state = Arc::new((Mutex::new(None), Condvar::new()));
    let reaper = Arc::clone(&state);
    thread::spawn(move || {
        wait_until_exited(pid);
        let (lock, done) = &*reaper;
        let mut result = lock.lock().unwrap_or_else(PoisonError::into_inner);
        *result = Some(child.wait().map_err(|err| err.to_string()));
        done.notify_all();
    });
    Supervised { pid, state }
}

/// Blocks until `pid` exits without reaping it (WNOWAIT).
fn wait_until_exited(pid: libc::pid_t) {
    loop {
        // SAFETY: waitid only writes into the zeroed siginfo we own.
        let rc = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT)
        };
        if rc == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

impl Supervised {
    pub fn kill(&self) {
        let result = self.state.0.lock().unwrap_or_else(PoisonError::into_inner);
        if result.is_none() {
            // SAFETY: the pid is unreaped (alive or a zombie) while the lock is held.
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
    }

    pub fn wait(&self) -> WaitResult {
        let (lock, done) = &*self.state;
        let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let guard = done.wait_while(guard, |r| r.is_none()).unwrap_or_else(PoisonError::into_inner);
        guard.clone().unwrap_or_else(|| Err("process state lost".to_string()))
    }

    pub fn wait_timeout(&self, timeout: Duration) -> Option<WaitResult> {
        let (lock, done) = &*self.state;
        let deadline = Instant::now() + timeout;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        while guard.is_none() {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            guard = done.wait_timeout(guard, remaining).unwrap_or_else(PoisonError::into_inner).0;
        }
        guard.clone()
    }
}

pub fn spawn(cmd: &mut Command) -> Result<Child, Error> {
    cmd.spawn().map_err(|err| Error::spawn(&cmd.get_program().to_string_lossy(), err))
}

/// Routes stdout and stderr into one pipe, so output keeps its interleaving.
pub fn merge_output(cmd: &mut Command) -> io::Result<PipeReader> {
    let (reader, writer) = io::pipe()?;
    cmd.stdin(Stdio::null()).stdout(writer.try_clone()?).stderr(writer);
    Ok(reader)
}

fn describe(cmd: &Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn read_in_background(source: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(mut source) = source {
            let _ = source.read_to_end(&mut out);
        }
        out
    })
}

/// Runs a bounded query and returns stdout; stderr is folded into the error
/// because a bare exit status explains nothing.
pub fn output(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>, Error> {
    let command = describe(&cmd);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = spawn(&mut cmd)?;
    drop(cmd);
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());
    let process = supervise(child);
    let Some(result) = process.wait_timeout(timeout) else {
        process.kill();
        return Err(Error::Timeout { command, after: timeout });
    };
    let status = result.map_err(|err| Error::Io(io::Error::other(err)))?;
    let out = stdout.join().unwrap_or_default();
    if status.success() {
        return Ok(out);
    }
    let stderr = stderr.join().unwrap_or_default();
    Err(Error::Exit { status, output: gocompat::decode_bytes(&stderr).trim().to_string() })
}

/// Runs to completion with stdout and stderr captured together; the output
/// explains a failure.
pub fn combined_output(mut cmd: Command) -> Result<(), Error> {
    let mut reader = merge_output(&mut cmd)?;
    let mut child = spawn(&mut cmd)?;
    drop(cmd);
    let mut output = Vec::new();
    reader.read_to_end(&mut output)?;
    let status = child.wait()?;
    if status.success() {
        return Ok(());
    }
    Err(Error::Exit { status, output: gocompat::decode_bytes(&output).trim().to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn output_includes_stderr() {
        let err = output(sh("echo out; echo 'daemon unreachable' >&2; exit 3"), Duration::from_secs(1)).unwrap_err();
        let Error::Exit { status, .. } = &err else { panic!("error = {err}") };
        assert_eq!(status.code(), Some(3));
        assert!(err.to_string().contains("daemon unreachable"), "{err}");
    }

    #[test]
    fn output_times_out() {
        let start = Instant::now();
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let err = output(cmd, Duration::from_millis(50)).unwrap_err();
        assert_eq!(err.to_string(), "sleep 5 timed out after 50ms");
        assert!(start.elapsed() < Duration::from_secs(2), "timeout took {:?}", start.elapsed());
    }

    #[test]
    fn output_success() {
        let out = output(sh("echo hi; echo noise >&2"), Duration::from_secs(1)).unwrap();
        assert_eq!(String::from_utf8_lossy(&out).trim(), "hi");
    }

    #[test]
    fn missing_binary_reads_like_go() {
        let err = output(Command::new("colimui-no-such-binary"), Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.to_string(), "exec: \"colimui-no-such-binary\": executable file not found in $PATH");
    }

    #[test]
    fn combined_output_reports_merged_output() {
        let err = combined_output(sh("echo a; echo b >&2; exit 1")).unwrap_err();
        assert_eq!(err.to_string(), "exit status 1: a\nb");
        assert!(combined_output(sh("true")).is_ok());
    }

    #[test]
    fn kill_after_exit_is_harmless() {
        let process = supervise(sh("exit 0").spawn().unwrap());
        assert!(process.wait().unwrap().success());
        process.kill();
        let process = supervise(Command::new("sleep").arg("5").spawn().unwrap());
        process.kill();
        assert!(!process.wait().unwrap().success());
    }
}
