//! Release checks and `colimuir update`: fetch the latest GitHub release,
//! verify its checksum, and replace the running binary. Releases share the Go
//! colimui repository, so only a release carrying a colimuir asset counts.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::Error;
use crate::model::{Cmd, Msg};
use crate::settings::write_file_atomic;
use crate::{NAME, VERSION};

const RELEASE_REPOSITORY: &str = "leodeim/colimui";
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/leodeim/colimui/releases/latest";
const MAX_RELEASE_JSON: u64 = 1 << 20;
const MAX_DOWNLOAD: u64 = 100 << 20;

pub fn check_for_update_cmd() -> Cmd {
    Cmd::run(|| {
        if VERSION == "dev" {
            return Msg::UpdateCheck(String::new());
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let offered = release_binary_name().and_then(|binary| Ok((latest_release(deadline)?, binary)));
        match offered {
            Ok((release, binary)) if release.is_update_for(&binary, VERSION) => Msg::UpdateCheck(release.tag),
            _ => Msg::UpdateCheck(String::new()),
        }
    })
}

/// Fetches `url` before `deadline`; a non-200 status fails with `failure`
/// followed by the status line.
fn get(url: &str, accept: Option<&str>, failure: &str, deadline: Instant, limit: u64) -> Result<Vec<u8>, Error> {
    let timeout = deadline.saturating_duration_since(Instant::now());
    let agent: ureq::Agent =
        ureq::Agent::config_builder().timeout_global(Some(timeout)).http_status_as_error(false).build().into();
    let mut request = agent.get(url);
    if let Some(accept) = accept {
        request = request.header("Accept", accept);
    }
    let mut response = request.call()?;
    let status = response.status();
    if status != ureq::http::StatusCode::OK {
        let status = format!("{} {}", status.as_u16(), status.canonical_reason().unwrap_or_default());
        return Err(Error::invalid(format!("{failure} {status}")));
    }
    let mut body = Vec::new();
    response.body_mut().as_reader().take(limit).read_to_end(&mut body)?;
    Ok(body)
}

#[derive(Debug, Deserialize)]
struct Release {
    #[serde(rename = "tag_name", default)]
    tag: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
}

impl Release {
    /// Newer than `current` and carrying `binary` plus its checksums.
    fn is_update_for(&self, binary: &str, current: &str) -> bool {
        let has = |name: &str| self.assets.iter().any(|asset| asset.name == name);
        is_newer_version(&self.tag, current) && has(binary) && has("checksums.txt")
    }
}

fn latest_release(deadline: Instant) -> Result<Release, Error> {
    let body =
        get(LATEST_RELEASE_URL, Some("application/vnd.github+json"), "GitHub returned", deadline, MAX_RELEASE_JSON)?;
    let release: Release = serde_json::from_slice(&body)?;
    if version_parts(&release.tag).is_none() {
        return Err(Error::invalid(format!("invalid release tag {:?}", release.tag)));
    }
    Ok(release)
}

pub fn is_newer_version(candidate: &str, current: &str) -> bool {
    match (version_parts(candidate), version_parts(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

fn version_parts(value: &str) -> Option<[u64; 3]> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let parts: Vec<&str> = value.split('.').collect();
    let [major, minor, patch] = parts.as_slice() else {
        return None;
    };
    Some([major.parse().ok()?, minor.parse().ok()?, patch.parse().ok()?])
}

pub fn self_update() -> Result<(), Error> {
    if VERSION == "dev" {
        return Err(Error::invalid("cannot update a development build"));
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let binary = release_binary_name()?;
    let release = latest_release(deadline)?;
    if !release.is_update_for(&binary, VERSION) {
        println!("{NAME} {VERSION} is already up to date");
        return Ok(());
    }
    let base = format!("https://github.com/{RELEASE_REPOSITORY}/releases/download/{}", release.tag);
    let data = get(&format!("{base}/{binary}"), None, "download failed:", deadline, MAX_DOWNLOAD)?;
    let checksums = get(&format!("{base}/checksums.txt"), None, "download failed:", deadline, MAX_DOWNLOAD)?;
    verify_checksum(&binary, &data, &String::from_utf8_lossy(&checksums))?;
    replace_executable(&data)?;
    println!("updated {NAME} to {}", release.tag);
    Ok(())
}

fn release_binary_name() -> Result<String, Error> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => {
            return Err(Error::invalid(format!("updates are not supported on {other}")));
        }
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => {
            return Err(Error::invalid(format!("updates are not supported on {os}/{other}")));
        }
    };
    Ok(format!("{NAME}_{os}_{arch}"))
}

pub fn verify_checksum(filename: &str, data: &[u8], checksums: &str) -> Result<(), Error> {
    let expected = checksums
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|fields| fields.len() >= 2 && fields.last() == Some(&filename))
        .map(|fields| fields[0].to_string())
        .ok_or_else(|| Error::invalid(format!("no checksum found for {filename}")))?;
    if !expected.eq_ignore_ascii_case(&hex::encode(Sha256::digest(data))) {
        return Err(Error::invalid(format!("checksum verification failed for {filename}")));
    }
    Ok(())
}

fn replace_executable(data: &[u8]) -> Result<(), Error> {
    let target = fs::canonicalize(std::env::current_exe()?)?;
    match replace_binary(&target, data) {
        Err(err) if err.is_permission_denied() => {}
        other => return other.map(drop),
    }
    let dir = target.parent().unwrap_or(Path::new("/")).display();
    eprintln!("{dir} is not writable by you, so sudo will replace it.");
    eprintln!("To update without a password, reinstall into a directory you own and remove this copy:");
    eprintln!("  sudo rm {}", target.display());
    sudo_install(&target, data)
}

/// Atomically swaps the binary behind `path`, following symlinks so a linked
/// install keeps its link; returns the resolved target.
pub fn replace_binary(path: &Path, data: &[u8]) -> Result<PathBuf, Error> {
    let target = fs::canonicalize(path)?;
    write_file_atomic(&target, data, 0o755)?;
    Ok(target)
}

fn sudo_install(target: &Path, data: &[u8]) -> Result<(), Error> {
    let mut temporary = tempfile::Builder::new().prefix("colimuir-update-").tempfile()?;
    temporary.write_all(data)?;
    temporary.flush()?;
    let status = Command::new("sudo")
        .args(["install", "-m", "0755"])
        .arg(temporary.path())
        .arg(target)
        .status()
        .map_err(|err| Error::spawn("sudo", err).context("replace executable"))?;
    if !status.success() {
        return Err(Error::Exit { status, output: String::new() }.context("replace executable"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn newer_version_comparison() {
        for (candidate, current, want) in [
            ("v0.0.2", "v0.0.1", true),
            ("v0.1.0", "v0.0.9", true),
            ("v1.0.0", "v1.0.0", false),
            ("v0.0.1", "v0.0.2", false),
            ("latest", "v0.0.1", false),
            ("v0.0.2", "dev", false),
        ] {
            assert_eq!(is_newer_version(candidate, current), want, "{candidate} vs {current}");
        }
    }

    fn release(tag: &str, assets: &[&str]) -> Release {
        Release { tag: tag.into(), assets: assets.iter().map(|name| Asset { name: name.to_string() }).collect() }
    }

    #[test]
    fn only_releases_with_a_colimuir_build_count() {
        let binary = "colimuir_darwin_arm64";
        assert!(release("v0.2.0", &[binary, "checksums.txt"]).is_update_for(binary, "v0.1.0"));
        assert!(!release("v0.2.0", &["colimui_darwin_arm64", "checksums.txt"]).is_update_for(binary, "v0.1.0"));
        assert!(!release("v0.2.0", &[binary]).is_update_for(binary, "v0.1.0"), "missing checksums");
        assert!(!release("v0.1.0", &[binary, "checksums.txt"]).is_update_for(binary, "v0.1.0"));
        assert!(!release("v0.2.0", &[binary, "checksums.txt"]).is_update_for(binary, "dev"));
        let parsed: Release =
            serde_json::from_str(r#"{"tag_name":"v0.1.13","assets":[{"name":"colimui_darwin_arm64"}]}"#).unwrap();
        assert!(!parsed.is_update_for(binary, "v0.1.0"), "a Go-only release must not look like a colimuir update");
    }

    #[test]
    fn checksum_verification() {
        let checksums = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  colimui_darwin_arm64\n";
        verify_checksum("colimui_darwin_arm64", b"hello", checksums).unwrap();
        assert!(verify_checksum("colimui_linux_amd64", b"hello", checksums).is_err());
        assert!(verify_checksum("colimui_darwin_arm64", b"tampered", checksums).is_err());
    }

    #[test]
    fn replace_binary_follows_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("colimui-bin");
        let link = dir.path().join("colimui");
        fs::write(&target, "old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let replaced = replace_binary(&link, b"new").unwrap();
        assert_eq!(replaced, fs::canonicalize(&target).unwrap());
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "link was replaced by a regular file");
        assert_eq!(fs::read_to_string(&link).unwrap(), "new");
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
    }

    #[test]
    fn replace_binary_reports_permission_without_leftovers() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("colimui");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let result = replace_binary(&path, b"new");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err_and(|err| err.is_permission_denied()));
        let entries = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!((entries, fs::read_to_string(&path).unwrap().as_str()), (1, "old"));
    }
}
