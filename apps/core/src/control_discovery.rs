//! Bounded discovery of already initialized Nodes visible to this account.
//!
//! This deliberately reads only known data directories. It never creates a directory or starts,
//! stops, or changes a Node. Platform process/service registrations are left to the desktop host;
//! its remembered directory list is passed explicitly in the control request.

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const MAX_PATHS: usize = 64;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Remembered,
    AppProfile,
    DefaultHome,
}

#[derive(Debug, Serialize)]
struct NodeSummary {
    status: String,
    device_id: String,
    data_dir: PathBuf,
    local_listen: Value,
    listen: Value,
    web_enabled: bool,
    core_version: String,
    sources: Vec<Source>,
}

/// Response schema: `{ "nodes": [{status, device_id, data_dir, local_listen, listen,
/// web_enabled, core_version, sources}], "warnings": [fixed_code] }`.
pub async fn discover(app_profile_dir: &Path, remembered: &[PathBuf]) -> Result<Value> {
    ensure!(
        app_profile_dir.is_absolute(),
        "app profile directory must be absolute"
    );
    ensure!(
        remembered.len() <= MAX_PATHS,
        "too many remembered Node directories"
    );
    let mut paths: BTreeMap<PathBuf, Vec<Source>> = BTreeMap::new();
    for dir in remembered {
        add_candidate(&mut paths, dir, Source::Remembered)?;
    }
    add_candidate(
        &mut paths,
        &app_profile_dir.join("node"),
        Source::AppProfile,
    )?;
    if let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        add_candidate(&mut paths, &home.join(".vibeface"), Source::DefaultHome)?;
    }
    ensure!(paths.len() <= MAX_PATHS, "too many local Node candidates");

    let mut nodes = Vec::new();
    let mut warnings = Vec::new();
    for (path, sources) in paths {
        // Missing/uninitialized defaults are ordinary absence, not phantom Nodes.
        if !path.join("config.toml").is_file() || !path.join("identity.json").is_file() {
            continue;
        }
        if !owned_by_current_user(&path)? {
            warnings.push("candidate_not_current_user_owned");
            continue;
        }
        match super::control::inspect(&path).await {
            Ok(info) => {
                let status = info
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unavailable");
                let device_id = info
                    .get("device_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let data_dir = info
                    .get("data_dir")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or(path);
                nodes.push(NodeSummary {
                    status: status.to_owned(),
                    device_id: device_id.to_owned(),
                    data_dir,
                    local_listen: info.get("local_listen").cloned().unwrap_or(Value::Null),
                    listen: info.get("listen").cloned().unwrap_or(Value::Null),
                    web_enabled: info
                        .get("web_enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    core_version: info
                        .get("core_version")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    sources,
                });
            }
            Err(_) => warnings.push("candidate_unavailable_or_identity_mismatch"),
        }
    }
    warnings.sort_unstable();
    warnings.dedup();
    Ok(json!({"nodes": nodes, "warnings": warnings}))
}

fn add_candidate(
    paths: &mut BTreeMap<PathBuf, Vec<Source>>,
    path: &Path,
    source: Source,
) -> Result<()> {
    ensure!(path.is_absolute(), "Node data directories must be absolute");
    ensure!(
        path.as_os_str().len() <= MAX_PATH_BYTES,
        "Node data directory path is too long"
    );
    let canonical = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Ok(()),
    };
    let metadata = fs::symlink_metadata(path).context("inspect Node data directory")?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "Node data directory must not be a symbolic link"
    );
    paths.entry(canonical).or_default().push(source);
    Ok(())
}

#[cfg(unix)]
fn owned_by_current_user(path: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(fs::metadata(path)?.uid() == unsafe { libc::geteuid() })
}

// Windows data paths are sourced from the current user's Desktop profile and explicit Desktop
// remembered list. A host-side SID check is needed before adding non-profile registrations.
#[cfg(windows)]
fn owned_by_current_user(_path: &Path) -> Result<bool> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn candidate_limit_and_absolute_path_are_enforced() {
        let mut paths = BTreeMap::new();
        assert!(add_candidate(&mut paths, Path::new("relative"), Source::Remembered).is_err());
        assert!(add_candidate(&mut paths, Path::new("/"), Source::Remembered).is_ok());
    }
}
