//! Bounded discovery of already initialized Nodes visible to this account.
//!
//! This reads only known data directories, one named user service per platform, and argv metadata
//! for the fixed Core executable. It never creates a directory or starts, stops, or changes a Node.

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const MAX_PATHS: usize = 64;
const MAX_PATH_BYTES: usize = 4096;

use vibeface_control::discovery::{
    Discovery, NodeSource as Source, NodeSummary, UnavailableReason,
};
use vibeface_control::inspection::{Inspection, NodeStatus};

/// Response schema: `{ "nodes": [{status, device_id, data_dir, sources, ...}],
/// "warnings": [fixed_code], "user_service_registered": bool }`.
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
    let user_service = match user_service_registration() {
        Ok(Some(service)) => {
            if !add_candidate(&mut paths, &service.data_dir, Source::UserService)? {
                warnings.push("candidate_limit_reached");
            }
            Some(service)
        }
        Ok(None) => None,
        Err(()) => {
            warnings.push("user_service_metadata_unavailable");
            None
        }
    };
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
    inspect_candidates(paths, warnings, user_service).await
}

async fn inspect_candidates(
    paths: BTreeMap<PathBuf, Vec<Source>>,
    mut warnings: Vec<&'static str>,
    user_service: Option<UserService>,
) -> Result<Value> {
    let mut nodes = Vec::new();
    for (path, sources) in paths {
        // Missing/uninitialized defaults are ordinary absence, not phantom Nodes.
        if !path.join("config.json").is_file() || !path.join("identity.json").is_file() {
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
                Ok(mut node) => {
                    if node.sources.contains(&Source::UserService) {
                        node.user_service_cli = user_service
                            .as_ref()
                            .and_then(|service| registered_cli(service, &node.data_dir).ok());
                    }
                    nodes.push(node);
                }
                Err(code) => warnings.push(code),
            },
            Err(_) => warnings.push("candidate_unavailable_or_identity_mismatch"),
        }
    }
    warnings.sort_unstable();
    warnings.dedup();
    let result = Discovery {
        nodes,
        warnings: warnings.into_iter().map(str::to_owned).collect(),
        user_service_registered: user_service.is_some(),
    };
    result.validate()?;
    Ok(serde_json::to_value(result)?)
}

fn summarize(
    sources: Vec<Source>,
    info: &Inspection,
) -> std::result::Result<NodeSummary, &'static str> {
    info.validate()
        .map_err(|_| "candidate_inspection_incomplete")?;
    let status = info.status;
    let device_id = &info.device_id;
    let data_dir = info.data_dir.clone();
    let (local_listen, listen, web_enabled, core_version, unavailable_reason) =
        if status == NodeStatus::Unavailable {
            (
                None,
                None,
                None,
                None,
                Some(match info.error.as_deref().unwrap_or("") {
                    error if error.contains("identity does not match") => {
                        UnavailableReason::IdentityMismatch
                    }
                    error if error.contains("maintenance") || error.contains("socket") => {
                        UnavailableReason::MaintenanceUnavailable
                    }
                    _ => UnavailableReason::InspectionUnavailable,
                }),
            )
        } else {
            (
                Some(info.local_listen),
                Some(info.listen),
                Some(info.web_enabled),
                Some(info.core_version.clone()),
                None,
            )
        };
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
        user_service_cli: None,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct UserService {
    cli: PathBuf,
    data_dir: PathBuf,
}

const CLI_BINARY: &str = if cfg!(windows) {
    "vibeface.exe"
} else {
    "vibeface"
};
const CORE_BINARY: &str = if cfg!(windows) {
    "vibeface-core.exe"
} else {
    "vibeface-core"
};
const SERVICE_SCRIPT: &str = if cfg!(windows) {
    "scripts/install-node-service.ps1"
} else {
    "scripts/install-node-service.sh"
};

fn parse_service_args(args: &[impl AsRef<std::ffi::OsStr>]) -> Option<UserService> {
    let [cli, verb, flag, data_dir] = args else {
        return None;
    };
    let cli = PathBuf::from(cli.as_ref());
    let data_dir = PathBuf::from(data_dir.as_ref());
    (verb.as_ref() == "serve"
        && flag.as_ref() == "--data-dir"
        && cli.is_absolute()
        && cli.file_name()?.to_str()?.eq_ignore_ascii_case(CLI_BINARY)
        && data_dir.is_absolute())
    .then_some(UserService { cli, data_dir })
}

fn registered_cli(service: &UserService, inspected_dir: &Path) -> Result<PathBuf> {
    use vibeface_bridge::platform::privatefs;

    ensure!(fs::canonicalize(&service.data_dir)? == inspected_dir);
    ensure!(privatefs::is_plain_file(&fs::symlink_metadata(
        &service.cli
    )?));
    let canonical_binary = fs::canonicalize(&service.cli)?;
    let package = canonical_binary
        .parent()
        .context("CLI package directory is missing")?;
    let file = |relative: &str| -> Result<PathBuf> {
        let path = package.join(relative);
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(privatefs::is_plain_file(&metadata));
        check_install_path(&path, package)?;
        Ok(path)
    };
    ensure!(privatefs::is_executable(&file(CLI_BINARY)?));
    ensure!(privatefs::is_executable(&file(CORE_BINARY)?));
    file(SERVICE_SCRIPT)?;
    let metadata: PackageBuild = read_metadata(&file("BUILD-INFO.json")?)?;

    let install_root = if let Some(vendor) = package
        .parent()
        .filter(|p| p.file_name().is_some_and(|name| name == "vendor"))
    {
        let root = vendor
            .parent()
            .context("npm package directory is missing")?;
        check_install_path(&root.join("package.json"), root)?;
        check_install_path(&root.join("bin/vibeface.mjs"), root)?;
        let npm: NpmPackage = read_metadata(&root.join("package.json"))?;
        ensure!(npm.name == "@leaperone/vibeface" && npm.version == metadata.version);
        ensure!(root.join("bin/vibeface.mjs").is_file());
        ensure!(
            fs::canonicalize(service.cli.parent().context("CLI directory is missing")?)? == package
        );
        root.to_owned()
    } else {
        let policy: InstallPolicy = serde_json::from_slice(&privatefs::read_private_bounded(
            &inspected_dir.join("update.json"),
            64 * 1024,
            "Node update policy",
        )?)?;
        ensure!(policy.install_root.is_absolute());
        let root = fs::canonicalize(policy.install_root)?;
        ensure!(package.starts_with(&root) && package != root);
        ensure!(fs::canonicalize(root.join("current").join(CLI_BINARY))? == canonical_binary);
        check_install_path(&root.join("current"), &root)?;
        check_install_path(&canonical_binary, &root)?;
        root
    };
    check_install_path(&service.cli, &install_root)?;
    Ok(service.cli.clone())
}

fn check_install_path(path: &Path, full_protection_from: &Path) -> Result<()> {
    for entry in path.ancestors() {
        let ancestor = entry != path && !entry.starts_with(full_protection_from);
        ensure!(
            vibeface_bridge::platform::privatefs::is_trusted_install_path(entry, ancestor)?,
            "registered CLI package is writable by another account"
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct PackageBuild {
    version: String,
}

#[derive(Deserialize)]
struct NpmPackage {
    name: String,
    version: String,
}

#[derive(Deserialize)]
struct InstallPolicy {
    install_root: PathBuf,
}

fn read_metadata<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    use std::io::Read;
    ensure!(vibeface_bridge::platform::privatefs::is_plain_file(
        &fs::symlink_metadata(path)?
    ));
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 64 * 1024);
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(unix)]
fn user_service_file(path: &Path, home: &Path) -> std::result::Result<bool, ()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(()),
    };
    if !vibeface_bridge::platform::privatefs::is_plain_file(&metadata)
        || metadata.permissions().mode() & 0o022 != 0
        || metadata.len() > 64 * 1024
        || !vibeface_bridge::platform::privatefs::same_owner(path, home).map_err(|_| ())?
    {
        return Err(());
    }
    Ok(true)
}

#[cfg(target_os = "macos")]
fn user_service_registration() -> std::result::Result<Option<UserService>, ()> {
    use plist::Value;
    let home = current_home().ok_or(())?;
    let plist = home.join("Library/LaunchAgents/com.leaperone.vibeface.node.plist");
    if !user_service_file(&plist, &home)? {
        return Ok(None);
    }
    let value = Value::from_file(plist).map_err(|_| ())?;
    let service = value.as_dictionary().ok_or(())?;
    if service.get("Label").and_then(Value::as_string) != Some("com.leaperone.vibeface.node")
        || service.contains_key("Program")
    {
        return Err(());
    }
    let args = service
        .get("ProgramArguments")
        .and_then(Value::as_array)
        .ok_or(())?;
    let args = args
        .iter()
        .map(|value| value.as_string().ok_or(()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    parse_service_args(&args).map(Some).ok_or(())
}

#[cfg(target_os = "linux")]
fn user_service_registration() -> std::result::Result<Option<UserService>, ()> {
    let home = current_home().ok_or(())?;
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    if !config.is_absolute() {
        return Err(());
    }
    let unit = config.join("systemd/user/vibeface.service");
    if !user_service_file(&unit, &home)? {
        return Ok(None);
    }
    let bytes = fs::read(unit).map_err(|_| ())?;
    if bytes.len() > 64 * 1024 {
        return Err(());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| ())?;
    parse_linux_service_unit(text).map(Some).ok_or(())
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn parse_linux_service_unit(text: &str) -> Option<UserService> {
    let mut starts = text
        .lines()
        .filter_map(|line| line.strip_prefix("ExecStart="));
    let (cli, tail) = systemd_argument(starts.next()?)?;
    if starts.next().is_some() {
        return None;
    }
    let (data_dir, end) = systemd_argument(tail.strip_prefix(" serve --data-dir ")?)?;
    end.is_empty().then_some(())?;
    parse_service_args(&[cli.as_str(), "serve", "--data-dir", data_dir.as_str()])
}

// Decode only the quoted syntax emitted by scripts/install-node-service.sh.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn systemd_argument(value: &str) -> Option<(String, &str)> {
    let mut chars = value.strip_prefix('"')?.chars();
    let mut output = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some((output, chars.as_str())),
            '\\' => {
                let escaped = chars.next()?;
                matches!(escaped, '\\' | '"').then_some(())?;
                output.push(escaped);
            }
            '%' | '$' => {
                (chars.next()? == ch).then_some(())?;
                output.push(ch);
            }
            ch if !ch.is_control() => output.push(ch),
            _ => return None,
        }
    }
    None
}

#[cfg(windows)]
fn user_service_registration() -> std::result::Result<Option<UserService>, ()> {
    let script = r#"
$ErrorActionPreference = 'Stop'
$id = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
try { $task = Get-ScheduledTask -TaskPath '\Vibeface\' -TaskName 'Node' }
catch {
    if ($_.CategoryInfo.Category -eq 'ObjectNotFound') { Write-Output 'null'; exit }
    throw
}
$owner = $task.Principal.UserId
if ($owner -notmatch '^S-1-') { $owner = ([Security.Principal.NTAccount]$owner).Translate([Security.Principal.SecurityIdentifier]).Value }
if ($owner -ne $id -or $task.Actions.Count -ne 1) { exit 1 }
$action = $task.Actions[0]
@{command=$action.Execute;arguments=$action.Arguments} | ConvertTo-Json -Compress
"#;
    let output = vibeface_bridge::platform::discovery::read_only_powershell(script, 16 * 1024)
        .map_err(|_| ())?;
    #[derive(Deserialize)]
    struct Action {
        command: String,
        arguments: String,
    }
    let Some(action): Option<Action> = serde_json::from_slice(&output).map_err(|_| ())? else {
        return Ok(None);
    };
    let combined = format!("\"{}\" {}", action.command, action.arguments);
    let args = vibeface_bridge::platform::discovery::command_line_args(&combined).ok_or(())?;
    parse_service_args(&args).map(Some).ok_or(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn user_service_registration() -> std::result::Result<Option<UserService>, ()> {
    Err(())
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
    // Windows can assign an elevated token's group as owner. The native private ACL check
    // already proves that only the current user is granted access and the token holds the owner.
    #[cfg(windows)]
    {
        vibeface_bridge::platform::privatefs::is_private(path)
    }
    #[cfg(not(windows))]
    {
        Ok(vibeface_bridge::platform::privatefs::owner_of(path)?
            == vibeface_bridge::platform::privatefs::current_account()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initialized_json_node_is_discovered_read_only() {
        let root =
            std::env::temp_dir().join(format!("vibeface-discovery-{}", uuid::Uuid::new_v4()));
        let dir = root.join("已有 Node");
        super::super::control::prepare_start(&dir).unwrap();
        let canonical = fs::canonicalize(&dir).unwrap();
        let config = fs::read(dir.join("config.json")).unwrap();
        let identity = fs::read(dir.join("identity.json")).unwrap();
        let epoch = fs::read(dir.join("data-epoch.json")).unwrap();
        let mut paths = BTreeMap::new();
        paths.insert(canonical.clone(), vec![Source::Remembered]);
        let observed = inspect_candidates(paths, Vec::new(), None).await.unwrap();
        assert_eq!(observed["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(observed["nodes"][0]["status"], "stopped");
        assert_eq!(observed["nodes"][0]["data_dir"], json!(canonical));
        assert_eq!(observed["nodes"][0]["sources"], json!(["remembered"]));
        assert_eq!(fs::read(dir.join("config.json")).unwrap(), config);
        assert_eq!(fs::read(dir.join("identity.json")).unwrap(), identity);
        assert_eq!(fs::read(dir.join("data-epoch.json")).unwrap(), epoch);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn private_node_on_the_workspace_volume_is_visible_to_its_account() {
        let root = std::env::current_dir()
            .unwrap()
            .join(".artifacts")
            .join(format!("vibeface-discovery-{}", uuid::Uuid::new_v4()));
        let dir = root.join("Node 数据");
        super::super::control::prepare_start(&dir).unwrap();
        assert!(vibeface_bridge::platform::privatefs::is_private(&dir).unwrap());
        assert!(owned_by_current_user(&dir).unwrap());
        let mut paths = BTreeMap::new();
        paths.insert(fs::canonicalize(&dir).unwrap(), vec![Source::Remembered]);
        let result = inspect_candidates(paths, Vec::new(), None).await.unwrap();
        assert_eq!(result["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(result["nodes"][0]["status"], "stopped");
        fs::remove_dir_all(root).unwrap();
    }

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

    #[cfg(unix)]
    #[test]
    fn systemd_fixture_extracts_only_known_node_and_spaced_path() {
        let fixture = "[Service]\nExecStart=\"/opt/Vibeface/vibeface\" serve --data-dir \"/home/user/Node 数据 2\"\n";
        assert_eq!(
            parse_linux_service_unit(fixture).unwrap(),
            UserService {
                cli: PathBuf::from("/opt/Vibeface/vibeface"),
                data_dir: PathBuf::from("/home/user/Node 数据 2"),
            }
        );
        assert!(
            parse_linux_service_unit(
                "[Service]\nExecStart=/tmp/unrelated serve --data-dir /tmp/node\n"
            )
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn registered_cli_must_be_the_managed_installation_bound_to_the_data_directory() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        use vibeface_bridge::platform::privatefs;

        let root =
            std::env::temp_dir().join(format!("vibeface-registered-cli-{}", uuid::Uuid::new_v4()));
        let data = root.join("node");
        let install = root.join("install");
        let package = install.join("packages/version");
        privatefs::create_dir(&data)?;
        fs::create_dir_all(package.join("scripts"))?;
        let observed = fs::canonicalize(&data)?;
        for path in [CLI_BINARY, CORE_BINARY, SERVICE_SCRIPT] {
            let path = package.join(path);
            fs::write(&path, "fixture")?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        }
        fs::write(package.join("BUILD-INFO.json"), br#"{"version":"1.0.0"}"#)?;
        symlink(&package, install.join("current"))?;
        let cli = install.join("current").join(CLI_BINARY);
        let service = UserService {
            cli: cli.clone(),
            data_dir: data.clone(),
        };
        assert!(registered_cli(&service, &observed).is_err());
        privatefs::write_atomic(
            &data.join("update.json"),
            &serde_json::to_vec(&json!({"install_root": install}))?,
        )?;
        assert_eq!(registered_cli(&service, &observed)?, cli);
        let release_dir = package.parent().unwrap();
        fs::set_permissions(release_dir, fs::Permissions::from_mode(0o777))?;
        assert!(registered_cli(&service, &observed).is_err());
        fs::set_permissions(release_dir, fs::Permissions::from_mode(0o755))?;
        assert!(registered_cli(&service, &fs::canonicalize(&root)?).is_err());
        let unrelated = root.join(CLI_BINARY);
        fs::copy(package.join(CLI_BINARY), &unrelated)?;
        let service = UserService {
            cli: unrelated,
            data_dir: data,
        };
        assert!(registered_cli(&service, &observed).is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn inspect_projection_distinguishes_healthy_stopped_and_identity_mismatch_without_raw_error() {
        let make = |status: &str, error: Value| {
            Inspection::from_value(json!({
                "status": status, "device_id":"a".repeat(64), "data_dir": std::env::temp_dir().join("controlled Node"),
                "local_listen":"127.0.0.1:4791", "listen":"127.0.0.1:4790",
                "web_enabled":true, "core_version":"0.0.1-fixture",
                "error": if status == "running" { Value::Null } else if error.is_null() { json!("fixture stopped") } else { error },
                "maintenance_socket": vibeface_bridge::platform::ipc::Endpoint::in_private_dir(&std::env::temp_dir(), "fixture.sock").unwrap().path(),
                "public_url":"http://127.0.0.1:4790", "http_enabled":status == "running",
                "version":(status == "running").then_some("0.0.1-fixture"), "write_state":null,
                "mesh":{"enabled":false,"network":"vibeface","records":null,"neighbors":null},
                "peers":(status == "running").then_some(Vec::<String>::new())
            })).unwrap()
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
