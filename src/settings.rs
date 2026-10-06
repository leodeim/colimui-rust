//! The persisted config file shared by the TUI and the menu bar item.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::model::Model;

/// The config file schema; values are written on toggle and loaded at
/// startup. Field order and omission match the Go version's output.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub auto_stop: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub log_timestamps: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub log_wrap: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menubar: Option<bool>,
}

impl Settings {
    /// Defaults to on when the key is absent (fresh installs and configs from
    /// before the menu bar existed); an explicit false sticks.
    pub fn menubar_enabled(&self) -> bool {
        self.menubar.unwrap_or(true)
    }
}

/// Follows XDG ($XDG_CONFIG_HOME, else ~/.config) on every platform, since
/// terminal tools live in ~/.config on macOS too. None when no home directory
/// resolves; persistence is then disabled and toggles apply to this run only.
pub fn settings_path() -> Option<PathBuf> {
    settings_path_from(std::env::var_os("XDG_CONFIG_HOME"), std::env::home_dir())
}

pub fn settings_path_from(xdg: Option<std::ffi::OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    let dir = match xdg.filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home?.join(".config"),
    };
    Some(dir.join("colimui").join("config.json"))
}

pub fn load_settings(path: Option<&Path>) -> Result<Settings, Error> {
    let Some(path) = path else {
        return Ok(Settings::default());
    };
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(err) => return Err(err.into()),
    };
    serde_json::from_slice(&data).map_err(|err| Error::Json(err).context(format!("parsing {}", path.display())))
}

/// Applies one change on top of the saved file so values written by other
/// toggles (or the menu bar process) are preserved.
pub fn update_settings(path: Option<&Path>, apply: impl FnOnce(&mut Settings)) -> Result<(), Error> {
    let Some(path) = path else { return Ok(()) };
    let mut settings = load_settings(Some(path))?;
    apply(&mut settings);
    let mut data = serde_json::to_vec_pretty(&settings)?;
    data.push(b'\n');
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    write_file_atomic(path, &data, 0o644)
}

/// Replaces `path` via a same-directory rename, so a concurrent reader (the
/// menu bar polls the config) or a crash never sees a partial file.
pub fn write_file_atomic(path: &Path, data: &[u8], mode: u32) -> Result<(), Error> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut tmp = tempfile::Builder::new().prefix(&format!(".{name}.")).tempfile_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().set_permissions(fs::Permissions::from_mode(mode))?;
    tmp.persist(path).map_err(|err| Error::Io(err.error))?;
    Ok(())
}

impl Model {
    /// Saves one settings change; on failure the toggle still applies to the
    /// current run and the footer reports it was not saved.
    pub fn persist_setting(&mut self, apply: impl FnOnce(&mut Settings)) {
        if let Err(err) = update_settings(self.settings_file.as_deref(), apply) {
            self.err = Some(err.to_string());
            self.status.push_str(" (not saved)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::model;
    use crate::update::shortcut_key;

    fn config_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("colimui").join("config.json")
    }

    #[test]
    fn settings_path_follows_xdg() {
        assert_eq!(
            settings_path_from(Some("/custom/config".into()), Some("/home/u".into())),
            Some(PathBuf::from("/custom/config/colimui/config.json"))
        );
        assert_eq!(
            settings_path_from(Some("".into()), Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.config/colimui/config.json"))
        );
        assert_eq!(settings_path_from(None, None), None);
    }

    #[test]
    fn load_settings_missing_and_malformed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_settings(Some(&dir.path().join("missing.json"))).unwrap(), Settings::default());
        let path = dir.path().join("config.json");
        fs::write(&path, "{").unwrap();
        let err = load_settings(Some(&path)).unwrap_err();
        assert!(err.to_string().contains(&path.display().to_string()), "{err}");
    }

    #[test]
    fn update_settings_preserves_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        update_settings(Some(&path), |s| s.auto_stop = "2h".into()).unwrap();
        update_settings(Some(&path), |s| s.log_wrap = true).unwrap();
        let s = load_settings(Some(&path)).unwrap();
        assert!(s.auto_stop == "2h" && s.log_wrap, "{s:?}");
    }

    #[test]
    fn writes_go_compatible_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        update_settings(Some(&path), |s| {
            s.auto_stop = "30m0s".into();
            s.log_wrap = true;
            s.menubar = Some(false);
        })
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\n  \"auto_stop\": \"30m0s\",\n  \"log_wrap\": true,\n  \"menubar\": false\n}\n"
        );
    }

    #[test]
    fn update_settings_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        for _ in 0..3 {
            update_settings(Some(&path), |s| s.log_wrap = !s.log_wrap).unwrap();
        }
        let entries: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(entries, ["config.json"]);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
    }

    #[test]
    fn log_toggle_keys_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        let mut m = model();
        m.settings_file = Some(path.clone());
        m.key(shortcut_key("T"));
        assert!(m.log_timestamps && m.status == "log timestamps on");
        m.key(shortcut_key("w"));
        assert!(m.log_wrap && m.status == "log wrap on");
        let s = load_settings(Some(&path)).unwrap();
        assert!(s.log_timestamps && s.log_wrap);
        m.key(shortcut_key("T"));
        let s = load_settings(Some(&path)).unwrap();
        assert!(!s.log_timestamps && s.log_wrap && m.status == "log timestamps off");
    }

    #[test]
    fn menubar_defaults_on_until_explicitly_disabled() {
        for (config, want) in [
            ("{}", true),
            (r#"{"log_wrap": true}"#, true),
            (r#"{"menubar": true}"#, true),
            (r#"{"menubar": false}"#, false),
        ] {
            let settings: Settings = serde_json::from_str(config).unwrap();
            assert_eq!(settings.menubar_enabled(), want, "{config}");
        }
    }

    #[test]
    fn menubar_disabled_survives_other_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        update_settings(Some(&path), |s| s.menubar = Some(false)).unwrap();
        update_settings(Some(&path), |s| s.log_wrap = true).unwrap();
        assert!(!load_settings(Some(&path)).unwrap().menubar_enabled());
    }
}
