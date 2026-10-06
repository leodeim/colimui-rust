//! colimuir: a lightweight terminal UI for Colima and Docker, ported from the
//! Go colimui.

mod ansi;
mod autostop;
mod backend;
mod clipboard;
mod error;
mod gocompat;
mod groups;
mod logs;
mod menubar_proc;
mod menubar_state;
#[cfg(target_os = "macos")]
mod menubar_tray;
mod model;
mod mouse;
mod overview;
mod process;
mod release;
mod settings;
mod stats;
mod style;
mod tea;
#[cfg(test)]
mod testutil;
mod update;
mod view;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use crate::autostop::{AUTO_STOP_ENV, resolve_auto_stop};
use crate::backend::ExecBackend;
use crate::error::Error;
use crate::menubar_proc::MENUBAR_SUPPORTED;
use crate::model::Model;
use crate::settings::{load_settings, settings_path};

/// The release tag in release builds (set via COLIMUI_VERSION at compile time).
pub const VERSION: &str = match option_env!("COLIMUI_VERSION") {
    Some(version) => version,
    None => "dev",
};

/// The program name shown to users, distinct from the Go `colimui`.
pub const NAME: &str = "colimuir";

const NO_COLOR_ENV: &str = "COLIMUI_NO_COLOR";

/// One subcommand; this table drives both dispatch and the help text.
struct CliCommand {
    names: &'static [&'static str],
    about: &'static str,
    run: fn(&mut dyn Write) -> Result<(), Error>,
}

const CLI_COMMANDS: &[CliCommand] = &[
    CliCommand { names: &["update"], about: "update to the latest release", run: |_| release::self_update() },
    CliCommand { names: &["menubar"], about: "run the macOS menu bar item", run: |_| run_menubar() },
    CliCommand {
        names: &["version", "--version", "-v"],
        about: "print the version",
        run: |out| Ok(writeln!(out, "{NAME} {VERSION}")?),
    },
    CliCommand {
        names: &["help", "--help", "-h"],
        about: "show this help",
        run: |out| Ok(out.write_all(usage().as_bytes())?),
    },
];

fn usage() -> String {
    let mut out = format!("{NAME} - terminal UI for Colima and Docker\n\nUsage:\n");
    out += &format!("  {NAME:<18} {}\n", "open the TUI");
    for c in CLI_COMMANDS {
        let mut about = c.about.to_string();
        if c.names.len() > 1 {
            about += &format!(" (also {})", c.names[1..].join(", "));
        }
        out += &format!("  {:<18} {about}\n", format!("{NAME} {}", c.names[0]));
    }
    out += "\nEnvironment:\n";
    out += &format!("  {:<18} {}\n", AUTO_STOP_ENV, "idle auto-stop for this run: a duration (45m, 2h) or off");
    out += &format!("  {:<18} {}\n", NO_COLOR_ENV, "set to 1 to disable colors");
    out
}

/// Dispatches command-line arguments and returns the exit code; usage errors
/// exit 2 so a typo never silently opens the TUI.
fn run_cli(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let Some(command) = CLI_COMMANDS.iter().find(|c| c.names.contains(&args[0].as_str())) else {
        let _ = write!(stderr, "{NAME}: unknown command {:?}\n\n{}", args[0], usage());
        return 2;
    };
    let name = command.names[0];
    if let Some(extra) = args.get(1) {
        let _ = writeln!(stderr, "{NAME} {name}: unexpected argument {extra:?}");
        return 2;
    }
    match (command.run)(stdout) {
        Ok(()) => 0,
        Err(err) => {
            let _ = writeln!(stderr, "{NAME} {name}: {err}");
            1
        }
    }
}

fn run_menubar() -> Result<(), Error> {
    #[cfg(target_os = "macos")]
    return menubar_tray::run(Arc::new(ExecBackend));
    #[cfg(not(target_os = "macos"))]
    Err(Error::invalid("the menubar requires a macOS build"))
}

/// Builds the TUI model from the saved config file and environment overrides.
fn configured_model(settings_file: Option<PathBuf>, auto_stop_env: &str) -> Result<Model, Error> {
    let mut m = Model::new(Arc::new(ExecBackend), None);
    let saved = load_settings(settings_file.as_deref())?;
    let path = settings_file.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
    m.settings_file = settings_file;
    m.log_timestamps = saved.log_timestamps;
    m.log_wrap = saved.log_wrap;
    m.menubar = saved.menubar_enabled();
    m.auto_stop_pinned = !auto_stop_env.trim().is_empty();
    (m.auto_stop_after, m.auto_stop) = resolve_auto_stop(auto_stop_env, &saved, &path)?;
    Ok(m)
}

fn main() {
    let args: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    if !args.is_empty() {
        std::process::exit(run_cli(&args, &mut io::stdout(), &mut io::stderr()));
    }
    style::enable_color(std::env::var(NO_COLOR_ENV).as_deref() != Ok("1"));
    let auto_stop_env = std::env::var(AUTO_STOP_ENV).unwrap_or_default();
    let mut m = match configured_model(settings_path(), &auto_stop_env) {
        Ok(m) => m,
        Err(err) => {
            eprintln!("{NAME}: {err}");
            std::process::exit(1);
        }
    };
    if m.menubar
        && MENUBAR_SUPPORTED
        && let Err(err) = menubar_proc::spawn_menubar()
    {
        m.err = Some(format!("menu bar item: {err}"));
    }
    if let Err(err) = tea::run(&mut m) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = run_cli(&args, &mut stdout, &mut stderr);
        (code, String::from_utf8(stdout).unwrap(), String::from_utf8(stderr).unwrap())
    }

    #[test]
    fn run_cli_dispatch() {
        let version = format!("{NAME} {VERSION}\n");
        for (args, code, stdout, stderr) in [
            (vec!["--version"], 0, version.as_str(), ""),
            (vec!["version"], 0, version.as_str(), ""),
            (vec!["-v"], 0, version.as_str(), ""),
            (vec!["--help"], 0, "Usage:", ""),
            (vec!["-h"], 0, "colimuir update", ""),
            (vec!["help"], 0, AUTO_STOP_ENV, ""),
            (vec!["udpate"], 2, "", "unknown command \"udpate\""),
            (vec!["version", "extra"], 2, "", "unexpected argument \"extra\""),
        ] {
            let (got_code, got_out, got_err) = cli(&args);
            assert_eq!(got_code, code, "{args:?} stderr {got_err:?}");
            assert!(got_out.contains(stdout) && got_err.contains(stderr), "{args:?}: {got_out:?} / {got_err:?}");
            if args == ["udpate"] {
                assert!(got_err.contains("Usage:"), "unknown command should print usage");
            }
            if args == ["version", "extra"] {
                assert!(!got_err.contains("Usage:"));
            }
        }
    }

    #[test]
    fn usage_lists_every_command() {
        let help = usage();
        for name in CLI_COMMANDS.iter().flat_map(|c| c.names) {
            assert!(help.contains(name), "usage is missing {name:?}");
        }
    }

    #[test]
    fn configured_model_reads_saved_settings_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("colimui").join("config.json");
        let m = configured_model(Some(path.clone()), "").unwrap();
        assert!(m.menubar && m.auto_stop && !m.auto_stop_pinned, "fresh install");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"menubar": false, "log_wrap": true, "auto_stop": "2h"}"#).unwrap();
        let m = configured_model(Some(path.clone()), "").unwrap();
        assert!(!m.menubar && m.log_wrap && m.auto_stop_after == std::time::Duration::from_secs(7200));
        let m = configured_model(Some(path.clone()), "off").unwrap();
        assert!(!m.auto_stop && m.auto_stop_pinned);
        assert!(configured_model(Some(path), "30s").is_err());
    }
}
