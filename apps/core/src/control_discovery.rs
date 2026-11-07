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

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Source {
    Remembered,
    AppProfile,
    DefaultHome,
    UserService,
    RunningProcess,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum UnavailableReason {
    IdentityMismatch,
    MaintenanceUnavailable,
    InspectionUnavailable,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum NodeStatus {
    Running,
    Stopped,
    Unavailable,
}

#[derive(Debug, Serialize)]
struct NodeSummary {
    status: NodeStatus,
    device_id: String,
    data_dir: PathBuf,
    local_listen: String,
    listen: String,
    web_enabled: bool,
    core_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_reason: Option<UnavailableReason>,
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
    let mut warnings = Vec::new();
    for dir in remembered {
        if !add_candidate(&mut paths, dir, Source::Remembered)? {
            return Err(anyhow::anyhow!("too many local Node candidates"));
        }
    }
    if !add_candidate(
        &mut paths,
        &app_profile_dir.join("node"),
        Source::AppProfile,
    )? {
        warnings.push("candidate_limit_reached");
    }
    if let Some(home) = current_home() {
        if !add_candidate(&mut paths, &home.join(".vibeface"), Source::DefaultHome)? {
            warnings.push("candidate_limit_reached");
        }
    } else {
        warnings.push("current_user_home_unavailable");
    }
    match user_service_data_dir() {
        Ok(Some(path)) => {
            if !add_candidate(&mut paths, &path, Source::UserService)? {
                warnings.push("candidate_limit_reached");
            }
        }
        Ok(None) => {}
        Err(()) => warnings.push("user_service_metadata_unavailable"),
    }
    match vibeface_bridge::platform::discovery::running_core_data_dirs() {
        Ok(found) => {
            for path in found {
                if !add_candidate(&mut paths, &path, Source::RunningProcess)? {
                    warnings.push("candidate_limit_reached");
                    break;
                }
            }
        }
        Err(_) => warnings.push("running_process_metadata_unavailable"),
    }
    ensure!(paths.len() <= MAX_PATHS, "too many local Node candidates");

    let mut nodes = Vec::new();
    for (path, sources) in paths {
        // Missing/uninitialized defaults are ordinary absence, not phantom Nodes.
        if !path.join("config.toml").is_file() || !path.join("identity.json").is_file() {
            continue;
        }
        match owned_by_current_user(&path) {
            Ok(true) => {}
            Ok(false) => {
                warnings.push("candidate_not_current_user_owned");
                continue;
            }
            Err(_) => {
                warnings.push("candidate_ownership_unavailable");
                continue;
            }
        }
        match super::control::inspect(&path).await {
            Ok(info) => match summarize(sources, &info) {
                Ok(node) => nodes.push(node),
                Err(code) => warnings.push(code),
            },
            Err(_) => warnings.push("candidate_unavailable_or_identity_mismatch"),
        }
    }
    warnings.sort_unstable();
    warnings.dedup();
    Ok(json!({"nodes": nodes, "warnings": warnings}))
}

fn summarize(sources: Vec<Source>, info: &Value) -> std::result::Result<NodeSummary, &'static str> {
    let status = match info.get("status").and_then(Value::as_str) {
        Some("running") => NodeStatus::Running,
        Some("stopped") => NodeStatus::Stopped,
        Some("unavailable") => NodeStatus::Unavailable,
        _ => return Err("candidate_inspection_incomplete"),
    };
    let device_id = info
        .get("device_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("candidate_inspection_incomplete")?;
    let data_dir = info
        .get("data_dir")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or("candidate_inspection_incomplete")?;
    let local_listen = info
        .get("local_listen")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or("candidate_inspection_incomplete")?;
    let listen = info
        .get("listen")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or("candidate_inspection_incomplete")?;
    let web_enabled = info
        .get("web_enabled")
        .and_then(Value::as_bool)
        .ok_or("candidate_inspection_incomplete")?;
    let core_version = info
        .get("core_version")
        .and_then(Value::as_str)
        .ok_or("candidate_inspection_incomplete")?
        .to_owned();
    let unavailable_reason = (status == NodeStatus::Unavailable).then(|| {
        match info.get("error").and_then(Value::as_str).unwrap_or("") {
            error if error.contains("identity does not match") => {
                UnavailableReason::IdentityMismatch
            }
            error if error.contains("maintenance") || error.contains("socket") => {
                UnavailableReason::MaintenanceUnavailable
            }
            _ => UnavailableReason::InspectionUnavailable,
        }
    });
    Ok(NodeSummary {
        status,
        device_id: device_id.to_owned(),
        data_dir,
        local_listen,
        listen,
        web_enabled,
        core_version,
        unavailable_reason,
        sources,
    })
}

#[cfg(target_os = "macos")]
fn user_service_data_dir() -> std::result::Result<Option<PathBuf>, ()> {
    use plist::Value;
    let home = current_home().ok_or(())?;
    let plist = home.join("Library/LaunchAgents/com.leaperone.vibeface.node.plist");
    let metadata = fs::symlink_metadata(&plist).map_err(|_| ())?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > 64 * 1024
        || !vibeface_bridge::platform::privatefs::same_owner(&plist, &home).map_err(|_| ())?
    {
        return Err(());
    }
    let value = Value::from_file(plist).map_err(|_| ())?;
    let args = value
        .as_dictionary()
        .and_then(|d| d.get("ProgramArguments"))
        .and_then(Value::as_array)
        .ok_or(())?;
    let args = args
        .iter()
        .map(|v| v.as_string().map(str::to_owned).ok_or(()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !known_server_args(&args) {
        return Err(());
    }
    Ok(data_dir_arg(&args).map(PathBuf::from))
}

#[cfg(target_os = "linux")]
fn user_service_data_dir() -> std::result::Result<Option<PathBuf>, ()> {
    let home = current_home().ok_or(())?;
    let unit = home.join(".config/systemd/user/vibeface.service");
    let metadata = fs::symlink_metadata(&unit).map_err(|_| ())?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > 64 * 1024
        || !vibeface_bridge::platform::privatefs::same_owner(&unit, &home).map_err(|_| ())?
    {
        return Err(());
    }
    let bytes = fs::read(unit).map_err(|_| ())?;
    if bytes.len() > 64 * 1024 {
        return Err(());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| ())?;
    parse_linux_service_unit(text)
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_service_unit(text: &str) -> std::result::Result<Option<PathBuf>, ()> {
    let starts = text
        .lines()
        .filter_map(|line| line.strip_prefix("ExecStart="))
        .collect::<Vec<_>>();
    if starts.len() != 1 {
        return Err(());
    }
    // Match systemd's known generated service form and its quoted --data-dir value. No shell is
    // involved, and escape decoding is limited to the escaping used by the service installer.
    let line = starts[0];
    let program = first_systemd_argument(line).ok_or(())?;
    if Path::new(program).file_name().and_then(|v| v.to_str()) != Some("vibeface-core")
        || !line.split_whitespace().any(|arg| arg == "serve")
        || line.matches("--data-dir ").count() != 1
    {
        return Err(());
    }
    let tail = line.rsplit_once("--data-dir \"").ok_or(())?.1;
    let encoded = tail.strip_suffix('"').ok_or(())?;
    let path = encoded
        .replace("\\\\", "\\")
        .replace("\\\"", "\"")
        .replace("%%", "%")
        .replace("$$", "$");
    let path = PathBuf::from(path);
    Ok(path.is_absolute().then_some(path))
}

#[cfg(any(target_os = "linux", test))]
fn first_systemd_argument(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if let Some(quoted) = line.strip_prefix('"') {
        return quoted.split_once('"').map(|(value, _)| value);
    }
    line.split_whitespace().next()
}

#[cfg(windows)]
fn user_service_data_dir() -> std::result::Result<Option<PathBuf>, ()> {
    let script = "$id=[Security.Principal.WindowsIdentity]::GetCurrent().User; $task=Get-ScheduledTask -TaskPath '\\Vibeface\\' -TaskName 'Node' -ErrorAction Stop; $owner=$task.Principal.UserId; if ($owner -notmatch '^S-1-') { $owner=([Security.Principal.NTAccount]$owner).Translate([Security.Principal.SecurityIdentifier]).Value }; $actions=@(); if ($owner -eq $id.Value) { foreach ($a in ($task.Actions | Select-Object -First 8)) { $actions+=@{sid=$owner;command=$a.Execute;arguments=$a.Arguments} } }; [pscustomobject]@{current_sid=$id.Value;actions=$actions} | ConvertTo-Json -Compress -Depth 3";
    let output = vibeface_bridge::platform::discovery::read_only_powershell(script, 16 * 1024)
        .map_err(|_| ())?;
    #[derive(serde::Deserialize)]
    struct Action {
        sid: String,
        command: String,
        arguments: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Task {
        current_sid: String,
        actions: Vec<Action>,
    }
    let task: Task = serde_json::from_slice(&output).map_err(|_| ())?;
    for action in task.actions.into_iter().take(8) {
        if action.sid != task.current_sid
            || !action
                .command
                .to_ascii_lowercase()
                .ends_with("vibeface-core.exe")
        {
            continue;
        }
        let combined = format!(
            "\"{}\" {}",
            action.command,
            action.arguments.unwrap_or_default()
        );
        if let Some(args) = vibeface_bridge::platform::discovery::command_line_args(&combined) {
            if args.iter().any(|arg| arg == "serve") {
                if let Some(path) = data_dir_arg_os(&args) {
                    return Ok(Some(path));
                }
            }
        }
    }
    Ok(None)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn user_service_data_dir() -> std::result::Result<Option<PathBuf>, ()> {
    Err(())
}

fn data_dir_arg(args: &[String]) -> Option<&str> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--data-dir" {
            return args
                .get(i + 1)
                .filter(|p| Path::new(p).is_absolute())
                .map(String::as_str);
        }
        if let Some(path) = args[i]
            .strip_prefix("--data-dir=")
            .filter(|p| Path::new(p).is_absolute())
        {
            return Some(path);
        }
        i += 1;
    }
    None
}

#[cfg(target_os = "macos")]
fn known_server_args(args: &[String]) -> bool {
    args.first()
        .and_then(|p| Path::new(p).file_name())
        .and_then(|n| n.to_str())
        == Some("vibeface-core")
        && args.iter().any(|a| a == "serve")
}

#[cfg(windows)]
fn data_dir_arg_os(args: &[std::ffi::OsString]) -> Option<PathBuf> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--data-dir" {
            let path = PathBuf::from(args.get(i + 1)?);
            return path.is_absolute().then_some(path);
        }
        if let Some(value) = args[i].to_str().and_then(|s| s.strip_prefix("--data-dir=")) {
            let path = PathBuf::from(value);
            return path.is_absolute().then_some(path);
        }
        i += 1;
    }
    None
}

fn current_home() -> Option<PathBuf> {
    #[cfg(windows)]
    let value = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let value = std::env::var_os("HOME");
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

fn add_candidate(
    paths: &mut BTreeMap<PathBuf, Vec<Source>>,
    path: &Path,
    source: Source,
) -> Result<bool> {
    ensure!(path.is_absolute(), "Node data directories must be absolute");
    ensure!(
        path.as_os_str().len() <= MAX_PATH_BYTES,
        "Node data directory path is too long"
    );
    let canonical = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(_) => return Ok(true),
    };
    let metadata = fs::symlink_metadata(path).context("inspect Node data directory")?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "Node data directory must not be a symbolic link"
    );
    if let Some(sources) = paths.get_mut(&canonical) {
        if !sources.contains(&source) {
            sources.push(source);
        }
        return Ok(true);
    }
    if paths.len() >= MAX_PATHS {
        return Ok(false);
    }
    paths.insert(canonical, vec![source]);
    Ok(true)
}

fn owned_by_current_user(path: &Path) -> Result<bool> {
    Ok(vibeface_bridge::platform::privatefs::owner_of(path)?
        == vibeface_bridge::platform::privatefs::current_account()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn candidate_limit_and_absolute_path_are_enforced() {
        let mut paths = BTreeMap::new();
        assert!(add_candidate(&mut paths, Path::new("relative"), Source::Remembered).is_err());
        let root =
            std::env::temp_dir().join(format!("vibeface-discovery-{}", uuid::Uuid::new_v4()));
        let node = root.join("node");
        fs::create_dir_all(&node).unwrap();
        assert!(add_candidate(&mut paths, &node, Source::Remembered).unwrap());
        let absent = root.join("not-initialized");
        assert!(add_candidate(&mut paths, &absent, Source::DefaultHome).unwrap());
        assert!(
            !absent.exists(),
            "discovery must not initialize candidate paths"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_fixture_extracts_unicode_spaced_data_dir() {
        let fixture = "[Service]\nExecStart=\"/opt/Vibeface/vibeface-core\" serve --data-dir \"/home/user/Node 数据 2\"\n";
        assert_eq!(
            parse_linux_service_unit(fixture).unwrap().unwrap(),
            PathBuf::from("/home/user/Node 数据 2")
        );
        assert!(
            parse_linux_service_unit(
                "[Service]\nExecStart=/tmp/unrelated serve --data-dir /tmp/node\n"
            )
            .is_err()
        );
    }

    #[test]
    fn systemd_fixture_extracts_only_known_node_and_spaced_path() {
        let fixture = "[Service]\nExecStart=\"/opt/Vibeface/vibeface-core\" serve --data-dir \"/home/user/Node 数据 2\"\n";
        assert_eq!(
            parse_linux_service_unit(fixture).unwrap().unwrap(),
            PathBuf::from("/home/user/Node 数据 2")
        );
        assert!(
            parse_linux_service_unit(
                "[Service]\nExecStart=/tmp/unrelated serve --data-dir /tmp/node\n"
            )
            .is_err()
        );
    }

    #[test]
    fn inspect_projection_distinguishes_healthy_stopped_and_identity_mismatch_without_raw_error() {
        let make = |status: &str, error: Value| {
            json!({
                "status": status, "device_id":"device-fixture", "data_dir":"/controlled/Node",
                "local_listen":"127.0.0.1:4791", "listen":"127.0.0.1:4790",
                "web_enabled":true, "core_version":"0.0.1-fixture", "error":error
            })
        };
        let running = summarize(vec![Source::Remembered], &make("running", Value::Null)).unwrap();
        let stopped = summarize(vec![Source::UserService], &make("stopped", Value::Null)).unwrap();
        let mismatched = summarize(
            vec![Source::RunningProcess],
            &make(
                "unavailable",
                json!("running node identity does not match this data directory"),
            ),
        )
        .unwrap();
        assert_eq!(running.status, NodeStatus::Running);
        assert_eq!(stopped.status, NodeStatus::Stopped);
        assert_eq!(
            mismatched.unavailable_reason,
            Some(UnavailableReason::IdentityMismatch)
        );
        let json = serde_json::to_value(mismatched).unwrap();
        assert_eq!(json["unavailable_reason"], "identity_mismatch");
        assert!(json.get("error").is_none());
    }
}
