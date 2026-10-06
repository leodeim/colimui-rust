//! Lifecycle of the `colimuir menubar` process: a locked pidfile keeps it a
//! singleton and lets the TUI find, start and stop it.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::autostop::AUTO_STOP_ENV;
use crate::error::Error;
use crate::settings::settings_path;

/// Gates the menu bar toggle to builds that can run it.
pub const MENUBAR_SUPPORTED: bool = cfg!(target_os = "macos");

/// The pidfile beside the config file; None when no config directory resolves.
pub fn menubar_pid_path() -> Option<PathBuf> {
    pid_path_for(&settings_path()?)
}

fn pid_path_for(config: &Path) -> Option<PathBuf> {
    Some(config.parent()?.join("menubar.pid"))
}

/// Takes the menu bar's exclusive lock and records our pid in the locked
/// file; the returned handle holds the lock until dropped.
pub fn lock_menubar(path: &Path) -> Result<File, Error> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    // A concurrent menubar_pid probe holds a shared lock for a few syscalls;
    // retry briefly so it cannot make a fresh start fail.
    let mut attempt = 0;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if attempt < 10 => {
                attempt += 1;
                thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => return Err(Error::MenubarRunning),
            Err(TryLockError::Error(err)) => return Err(err.into()),
        }
    }
    file.set_len(0)?;
    file.write_all_at(format!("{}\n", std::process::id()).as_bytes(), 0)?;
    Ok(file)
}

/// The pid of the process holding the menu bar lock; an unlocked, missing or
/// unreadable pidfile counts as not running, which also guards against pid
/// reuse after a crash.
pub fn menubar_pid(path: &Path) -> Option<i32> {
    let mut file = File::open(path).ok()?;
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            return None;
        }
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(_)) => return None,
    }
    let mut data = String::new();
    file.read_to_string(&mut data).ok()?;
    data.trim().parse::<i32>().ok().filter(|pid| *pid > 0)
}

/// Starts `colimuir menubar` in its own session so it survives the TUI
/// exiting; a no-op when one is already running.
pub fn spawn_menubar() -> Result<(), Error> {
    if menubar_alive() {
        return Ok(());
    }
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("menubar").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // The menu bar outlives this run, so it follows the saved setting rather
    // than this run's COLIMUI_AUTO_STOP override.
    cmd.env_remove(AUTO_STOP_ENV);
    // SAFETY: setsid is async-signal-safe and touches no parent state.
    unsafe {
        cmd.pre_exec(|| if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    cmd.spawn().map_err(|err| Error::spawn(crate::NAME, err))?;
    Ok(())
}

/// Whether a menu bar process is running; while it is, it owns idle
/// auto-stop and the TUI must not dispatch stops of its own.
pub fn menubar_alive() -> bool {
    menubar_pid_path().and_then(|path| menubar_pid(&path)).is_some()
}

pub fn stop_menubar() -> Result<(), Error> {
    stop_menubar_at(menubar_pid_path().as_deref())
}

fn stop_menubar_at(path: Option<&Path>) -> Result<(), Error> {
    let Some(pid) = path.and_then(menubar_pid) else {
        return Ok(());
    };
    // SAFETY: kill only sends a signal to the pid that holds the lock.
    if unsafe { libc::kill(pid, libc::SIGTERM) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid_path(dir: &tempfile::TempDir) -> PathBuf {
        pid_path_for(&dir.path().join("colimui").join("config.json")).unwrap()
    }

    #[test]
    fn menubar_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = pid_path(&dir);
        assert_eq!(menubar_pid(&path), None, "no pidfile should mean not running");
        let lock = lock_menubar(&path).unwrap();
        assert_eq!(menubar_pid(&path), Some(std::process::id() as i32));
        assert!(matches!(lock_menubar(&path), Err(Error::MenubarRunning)));
        drop(lock);
        assert_eq!(menubar_pid(&path), None, "released lock should mean not running");
        lock_menubar(&path).expect("relock after release");
    }

    #[test]
    fn menubar_pid_ignores_unlocked_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = pid_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for content in [format!("{}\n", std::process::id()), "not-a-pid\n".into(), "-4\n".into()] {
            fs::write(&path, &content).unwrap();
            assert_eq!(menubar_pid(&path), None, "unlocked pidfile {content:?} should mean not running");
            stop_menubar_at(Some(&path)).expect("stop on unlocked pidfile should be a no-op");
        }
    }
}
