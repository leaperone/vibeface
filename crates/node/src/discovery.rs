//! Finds the paths this computer's providers need and writes them into the Node's own
//! configuration once, where Settings shows them and the owner can change them. Nothing is
//! guessed while running: a value is only filled in when it is missing for a use that is on.
//! The paths stay in `config.json` and `provider-env.json`; they never reach shared records.
use crate::{
    config::{Config, ManagerSettings},
    providers::{AppServer, AppServerMode, Local, Provider, ProviderEnv},
    triage::TriageConfig,
};
use anyhow::{Result, bail};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
};

pub const CODEX_HOME: &str = "CODEX_HOME";

/// Codex is not on this computer: something the owner sets up, not a failure, so the client
/// channel tags it with [`CodexMissing::CODE`].
#[derive(Debug)]
pub struct CodexMissing;
impl CodexMissing {
    pub const CODE: &str = "codex_missing";
}
impl std::fmt::Display for CodexMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "no Codex program found on this computer: install Codex or set its program path",
        )
    }
}
impl std::error::Error for CodexMissing {}

/// What the Node's own environment says about where things are on this computer.
#[derive(Clone, Debug, Default)]
pub struct Host {
    pub codex_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub path: Option<OsString>,
}

impl Host {
    /// The Node process's environment. Unit tests see an empty one, so they never reach the
    /// operator's own Codex.
    pub fn current() -> Self {
        if cfg!(test) {
            return Self::default();
        }
        let dir = |name| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        Self {
            codex_home: dir(CODEX_HOME),
            home: dir("HOME"),
            path: std::env::var_os("PATH"),
        }
    }

    /// Whether `program` is on this computer: it runs as configured, or discovery would find it.
    pub fn installed(&self, program: &Path) -> bool {
        runnable(program, self.path.as_ref())
            || bare(program).and_then(|name| self.program(name)).is_some()
    }

    /// A program's absolute path: from the Node's `PATH`, then where installers put programs
    /// that a service's short `PATH` leaves out.
    pub fn program(&self, name: &str) -> Option<PathBuf> {
        let mut dirs: Vec<PathBuf> = self
            .path
            .iter()
            .flat_map(std::env::split_paths)
            .filter(|dir| dir.is_absolute())
            .collect();
        let home = |dir: &str| self.home.as_ref().map(|home| home.join(dir));
        dirs.extend(home(".local/bin"));
        // Where Claude Code's and opencode's own installers put them.
        // ponytail: pi comes from npm, found here only in Homebrew's or the official Node's
        // global folder; another one (nvm, a custom prefix) needs PATH or a chosen path.
        dirs.extend(match name {
            "claude" => home(".claude/local"),
            "opencode" => home(".opencode/bin"),
            _ => None,
        });
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
        if name == "codex" {
            dirs.push("/Applications/Codex.app/Contents/Resources".into());
        }
        dirs.into_iter()
            .map(|dir| dir.join(name))
            .find(|path| executable_file(path))
    }

    /// `path` as the owner reads it in a question: under the home folder as `~/…`, elsewhere
    /// only its last two parts, so the full path stays on this computer.
    /// The usual project folders in the home folder that exist, for a first folder to offer.
    pub fn project_folders(&self) -> Vec<PathBuf> {
        let Some(home) = &self.home else {
            return Vec::new();
        };
        let mut found: Vec<PathBuf> = [
            "projects",
            "Developer",
            "code",
            "src",
            "dev",
            "repos",
            "workspace",
            "work",
        ]
        .iter()
        .filter_map(|name| home.join(name).canonicalize().ok())
        .filter(|path| path.is_dir())
        .collect();
        found.dedup();
        found
    }

    pub fn display(&self, path: &Path) -> String {
        self.tilde(path).unwrap_or_else(|| {
            let parts: Vec<_> = path.components().rev().take(2).collect();
            let tail: PathBuf = parts.into_iter().rev().collect();
            format!("…/{}", tail.display())
        })
    }

    /// `path` under the home folder, written from `~`.
    pub fn tilde(&self, path: &Path) -> Option<String> {
        let rest = path.strip_prefix(self.home.as_ref()?).ok()?;
        Some(match rest.as_os_str().is_empty() {
            true => "~".into(),
            false => format!("~/{}", rest.display()),
        })
    }

    /// A path the owner typed, with a leading `~` read as the home folder.
    pub fn expand(&self, path: &Path) -> PathBuf {
        match (path.strip_prefix("~"), &self.home) {
            (Ok(rest), Some(home)) if rest.as_os_str().is_empty() => home.clone(),
            (Ok(rest), Some(home)) => home.join(rest),
            _ => path.to_path_buf(),
        }
    }

    /// Every absolute path string in `value` that lies under the home folder, with how
    /// Settings shows it.
    pub fn display_paths(&self, value: &serde_json::Value) -> BTreeMap<String, String> {
        let mut shown = BTreeMap::new();
        let mut pending = vec![value];
        while let Some(value) = pending.pop() {
            match value {
                serde_json::Value::String(text) if text.starts_with('/') => {
                    if let Some(short) = self.tilde(Path::new(text)) {
                        shown.insert(text.clone(), short);
                    }
                }
                serde_json::Value::Array(items) => pending.extend(items),
                serde_json::Value::Object(fields) => pending.extend(fields.values()),
                _ => {}
            }
        }
        shown
    }
}

fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Whether `program` runs as configured: a path that is an executable file, or a bare name
/// found on the Node's `PATH`.
pub fn runnable(program: &Path, path: Option<&OsString>) -> bool {
    if program.components().count() != 1 || program.is_absolute() {
        return executable_file(program);
    }
    path.is_some_and(|paths| {
        std::env::split_paths(paths).any(|dir| executable_file(&dir.join(program)))
    })
}

/// Where Codex's running App Server listens: the desktop app and `codex app-server proxy` keep
/// this link to the live socket.
pub fn control_socket(codex_home: &Path) -> Option<PathBuf> {
    let socket = codex_home.join("app-server-control/app-server-control.sock");
    socket.symlink_metadata().is_ok().then_some(socket)
}

pub fn sessions_dir(codex_home: &Path) -> Option<PathBuf> {
    let sessions = codex_home.join("sessions");
    sessions.is_dir().then_some(sessions)
}

/// Provider paths for this device, each `None` when it stays as it is, and the providers to
/// turn execution on for. Variables with keys never pass through here.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Paths {
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub execution: BTreeSet<Provider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_server: Option<AppServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<PathBuf>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub bins: BTreeMap<Provider, PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_sessions: Option<PathBuf>,
}

/// The parts of this device's configuration that provider paths live in.
pub struct Target<'a> {
    pub local: &'a mut Local,
    pub manager: &'a mut Option<ManagerSettings>,
    pub triage: &'a mut TriageConfig,
}

impl Paths {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Which settings this sets, for logs that must not hold the paths.
    pub fn names(&self) -> Vec<String> {
        [
            ("execution", !self.execution.is_empty()),
            ("app_server", self.app_server.is_some()),
            ("codex_home", self.codex_home.is_some()),
            ("codex_sessions", self.codex_sessions.is_some()),
        ]
        .into_iter()
        .filter(|(_, set)| *set)
        .map(|(name, _)| name.to_owned())
        .chain(
            self.bins
                .keys()
                .map(|provider| format!("{}_bin", provider.name())),
        )
        .collect()
    }

    /// `other`'s values for what this leaves unset.
    pub fn or(self, other: Paths) -> Paths {
        let mut bins = other.bins;
        bins.extend(self.bins);
        let mut execution = other.execution;
        execution.extend(self.execution);
        Paths {
            execution,
            app_server: self.app_server.or(other.app_server),
            codex_home: self.codex_home.or(other.codex_home),
            bins,
            codex_sessions: self.codex_sessions.or(other.codex_sessions),
        }
    }

    /// Only what differs from `target`.
    pub fn changes(self, target: &Target) -> Paths {
        let differs = |value: Option<PathBuf>, current: Option<&PathBuf>| {
            value.filter(|value| Some(value) != current)
        };
        Paths {
            execution: self
                .execution
                .difference(&target.local.switches.execution)
                .copied()
                .collect(),
            app_server: self
                .app_server
                .filter(|server| *server != target.local.switches.codex_app_server),
            codex_home: self.codex_home.filter(|home| {
                target.local.env.get(Provider::Codex).get(CODEX_HOME)
                    != Some(&home.to_string_lossy().into_owned())
            }),
            bins: self
                .bins
                .into_iter()
                .filter(|(provider, bin)| *bin != target.local.bin(*provider))
                .collect(),
            codex_sessions: differs(self.codex_sessions, target.triage.codex_sessions.as_ref()),
        }
    }

    pub fn apply(&self, target: Target) -> Result<()> {
        target
            .local
            .switches
            .execution
            .extend(self.execution.iter().copied());
        if let Some(server) = &self.app_server {
            target.local.switches.codex_app_server = server.clone();
        }
        if let Some(home) = &self.codex_home {
            target.local.env.apply(
                Provider::Codex,
                BTreeMap::from([(CODEX_HOME.into(), Some(home.to_string_lossy().into_owned()))]),
            )?;
        }
        target.local.bins.extend(self.bins.clone());
        if let Some(sessions) = &self.codex_sessions {
            target.triage.codex_sessions = Some(sessions.clone());
        }
        Ok(())
    }
}

/// What this computer supplies for the uses that are on and miss a program path. `codex` also
/// counts Codex as in use when it is about to be. External session scanning needs an explicit root.
pub fn missing(target: &Target, codex: bool, host: &Host) -> Result<Paths> {
    let local = &*target.local;
    let codex = codex
        || local.switches.app_server_used(
            target
                .manager
                .as_ref()
                .is_some_and(|m| m.provider.on_app_server()),
        );
    let mut paths = Paths::default();
    let server = &local.switches.codex_app_server;
    let spawn = server.mode == AppServerMode::Spawn;
    // A missing socket never means "find the user's App Server". The Node injects its own
    // provider root at runtime; a system server is an explicit socket-only opt-in.
    if target.triage.codex_sessions.is_none() {
        if target.triage.scan_codex_sessions {
            bail!("set an explicit Codex sessions directory before scanning external sessions");
        }
    }
    let bot = target.manager.as_ref().map(|manager| manager.provider);
    for provider in Provider::ALL {
        let launches = match provider.on_app_server() {
            true => codex && spawn,
            false => bot == Some(provider) || local.switches.execution.contains(&provider),
        };
        let bin = local.bin(provider);
        if launches
            && !runnable(&bin, host.path.as_ref())
            && let Some(found) = bare(&bin).and_then(|name| host.program(name))
        {
            paths.bins.insert(provider, found);
        }
    }
    Ok(paths)
}

/// Fills what `config` and `env` miss for the uses they turn on. `init` runs it before the
/// first save, and a Node before it starts with a saved configuration.
pub fn complete(config: &mut Config, env: &mut ProviderEnv, host: &Host) -> Result<Paths> {
    let mut local = Local {
        switches: config.providers.clone(),
        bins: std::mem::take(&mut config.bins),
        env: std::mem::take(env),
    };
    let filled = missing(
        &Target {
            local: &mut local,
            manager: &mut config.manager,
            triage: &mut config.triage,
        },
        false,
        host,
    );
    if let Ok(filled) = &filled {
        filled.apply(Target {
            local: &mut local,
            manager: &mut config.manager,
            triage: &mut config.triage,
        })?;
    }
    config.providers = local.switches;
    config.bins = local.bins;
    *env = local.env;
    filled
}

/// Before a Node starts: what its saved configuration misses is found and saved. When Codex
/// cannot be found, nothing is written and loading the configuration says why.
pub fn complete_saved(dir: &Path, host: &Host) -> Result<()> {
    let mut config: Config = serde_json::from_slice(&std::fs::read(dir.join("config.json"))?)?;
    let mut env = ProviderEnv::load(dir)?;
    let filled = match complete(&mut config, &mut env, host) {
        Ok(filled) => filled,
        Err(error) => {
            tracing::warn!(event = "providers.missing", %error, "provider paths are missing and could not be found");
            return Ok(());
        }
    };
    if filled.is_empty() {
        return Ok(());
    }
    config.save(dir)?;
    if filled.codex_home.is_some() {
        env.save(dir)?;
    }
    tracing::info!(event = "providers.discovered", filled = ?filled.names(), "filled missing provider paths from this computer");
    Ok(())
}

/// A program given by name only; a path the owner chose is never replaced.
fn bare(program: &Path) -> Option<&str> {
    (program.components().count() == 1 && !program.is_absolute())
        .then(|| program.to_str())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Providers;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vibeface-discovery-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn local() -> Local {
        Local {
            switches: Providers::default(),
            bins: BTreeMap::new(),
            env: ProviderEnv::default(),
        }
    }

    fn program(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn settings_show_home_paths_from_tilde_and_read_typed_tilde_as_home() {
        let host = Host {
            codex_home: None,
            home: Some("/Users/a".into()),
            path: None,
        };
        let response = serde_json::json!({
            "roots": ["/Users/a/src", "/srv/shared"],
            "providers": [{"home": "/Users/a/.codex", "bin": "codex", "socket": null,
                "issues": [{"path": "/Users/ab/codex"}]}],
            "workspace": "/Users/a",
        });
        assert_eq!(
            host.display_paths(&response),
            BTreeMap::from([
                ("/Users/a".into(), "~".into()),
                ("/Users/a/.codex".into(), "~/.codex".into()),
                ("/Users/a/src".into(), "~/src".into()),
            ]),
            "only paths under the home folder get a short form; a sibling folder is not under it"
        );
        assert_eq!(
            host.expand(Path::new("~/.codex/app.sock")),
            PathBuf::from("/Users/a/.codex/app.sock")
        );
        assert_eq!(host.expand(Path::new("~")), PathBuf::from("/Users/a"));
        for kept in ["/opt/codex", "~other/x", "codex"] {
            assert_eq!(host.expand(Path::new(kept)), PathBuf::from(kept));
        }
        assert_eq!(
            Host::default().expand(Path::new("~/x")),
            PathBuf::from("~/x"),
            "without a home folder the path stays as typed and fails as not absolute"
        );
    }

    #[test]
    fn missing_discovery_never_adopts_a_user_codex_folder() -> Result<()> {
        let root = scratch("none");
        let host = Host {
            codex_home: None,
            home: Some(root.clone()),
            path: None,
        };
        let (mut local, mut manager, mut triage) = (local(), None, TriageConfig::default());
        let target = Target {
            local: &mut local,
            manager: &mut manager,
            triage: &mut triage,
        };
        assert!(missing(&target, true, &host)?.is_empty());
        assert!(
            missing(&target, false, &host)?.is_empty(),
            "nothing is looked up while Codex is not in use"
        );
        assert!(missing(&target, true, &Host::default())?.is_empty());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn missing_system_socket_never_adopts_the_users_app_server() -> Result<()> {
        let root = scratch("socket");
        let codex = root.join(".codex");
        std::fs::create_dir_all(codex.join("sessions"))?;
        let host = Host {
            codex_home: None,
            home: Some(root.clone()),
            path: None,
        };
        let (mut local, mut manager, mut triage) = (local(), None, TriageConfig::default());
        let found = missing(
            &Target {
                local: &mut local,
                manager: &mut manager,
                triage: &mut triage,
            },
            true,
            &host,
        )?;
        assert_eq!(found.app_server, None);
        assert_eq!(found.codex_home, None);
        assert_eq!(found.codex_sessions, None);

        std::fs::create_dir_all(codex.join("app-server-control"))?;
        let socket = codex.join("app-server-control/app-server-control.sock");
        std::os::unix::fs::symlink(root.join("gone.sock"), &socket)?;
        let found = missing(
            &Target {
                local: &mut local,
                manager: &mut manager,
                triage: &mut triage,
            },
            true,
            &host,
        )?;
        assert_eq!(found.app_server, None);
        assert_eq!(found.codex_home, None);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn startup_does_not_save_user_provider_paths() -> Result<()> {
        let root = scratch("startup");
        let codex = root.join(".codex");
        std::fs::create_dir_all(codex.join("sessions"))?;
        let dir = root.join("node");
        crate::config::private_dir(&dir)?;
        // Saved by a Node before discovery: execution on, nothing to run it with.
        let mut config =
            serde_json::to_value(Config::local("127.0.0.1:0".parse()?, vec![], false))?;
        config["providers"]["execution"] = serde_json::json!(["codex"]);
        std::fs::write(dir.join("config.json"), serde_json::to_vec(&config)?)?;
        let host = Host {
            codex_home: None,
            home: Some(root.clone()),
            path: None,
        };
        complete_saved(&dir, &host)?;
        let saved = Config::load(&dir)?;
        assert_eq!(saved.providers.codex_app_server.mode, AppServerMode::Spawn);
        assert_eq!(saved.triage.codex_sessions, None);
        assert!(
            ProviderEnv::load(&dir)?
                .get(crate::providers::Provider::Codex)
                .get(CODEX_HOME)
                .is_none()
        );
        let before = std::fs::read(dir.join("config.json"))?;
        complete_saved(&dir, &host)?;
        assert_eq!(
            std::fs::read(dir.join("config.json"))?,
            before,
            "a saved path is not looked up again"
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn programs_named_only_are_found_where_a_service_path_does_not_look() -> Result<()> {
        let root = scratch("programs");
        let bridge = root.join(".local/bin/cursor-sdk-bridge");
        program(&bridge);
        let host = Host {
            codex_home: None,
            home: Some(root.clone()),
            path: Some("/usr/bin:/bin".into()),
        };
        let mut manager = Some(ManagerSettings {
            provider: Provider::Cursor,
            workspace: root.clone(),
            model: "auto".into(),
            reasoning_effort: None,
            backup: Default::default(),
        });
        let (mut local, mut triage) = (local(), TriageConfig::default());
        let found = missing(
            &Target {
                local: &mut local,
                manager: &mut manager,
                triage: &mut triage,
            },
            false,
            &host,
        )?;
        assert_eq!(found.bins, BTreeMap::from([(Provider::Cursor, bridge)]));
        local
            .bins
            .insert(Provider::Cursor, root.join("chosen/cursor-sdk-bridge"));
        assert!(
            missing(
                &Target {
                    local: &mut local,
                    manager: &mut manager,
                    triage: &mut triage
                },
                false,
                &host,
            )?
            .is_empty(),
            "a path the owner chose stays, even when it is gone"
        );
        assert_eq!(host.display(&root.join(".codex")), "~/.codex");
        assert_eq!(
            host.display(Path::new("/opt/tools/bin/codex")),
            "…/bin/codex"
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn claude_code_and_opencode_are_found_where_their_installers_put_them() {
        let root = scratch("installers");
        let host = Host {
            codex_home: None,
            home: Some(root.clone()),
            path: Some("/usr/bin:/bin".into()),
        };
        for path in [".claude/local/claude", ".opencode/bin/opencode"].map(|p| root.join(p)) {
            program(&path);
            let name = path.file_name().unwrap().to_str().unwrap();
            assert_eq!(host.program(name), Some(path.clone()));
        }
        let native = root.join(".local/bin/claude");
        program(&native);
        assert_eq!(
            host.program("claude"),
            Some(native),
            "the native install comes first"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
