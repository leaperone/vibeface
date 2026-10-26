//! Finds the paths this computer's providers need and writes them into the Node's own
//! configuration once, where Settings shows them and the owner can change them. Nothing is
//! guessed while running: a value is only filled in when it is missing for a use that is on.
//! The paths stay in `config.json` and `provider-env.json`; they never reach shared records.
use crate::{
    config::{Config, ManagerSettings},
    manager::ManagerKind,
    providers::{AppServer, AppServerMode, Local, Provider, ProviderEnv},
    triage::TriageConfig,
};
use anyhow::{Result, bail};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

pub const CODEX_HOME: &str = "CODEX_HOME";
const NOT_FOUND: &str = "no Codex folder found on this computer: install Codex so $HOME/.codex exists, set CODEX_HOME for the Node, or enter the Codex folder";

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

    /// `CODEX_HOME` of the Node's environment when set, else `$HOME/.codex`; it must exist.
    pub fn codex_home(&self) -> Result<PathBuf> {
        let home = match &self.codex_home {
            Some(home) => home.clone(),
            None => match &self.home {
                Some(home) => home.join(".codex"),
                None => bail!(NOT_FOUND),
            },
        };
        if home.is_absolute() && home.is_dir() {
            Ok(home)
        } else {
            bail!(NOT_FOUND)
        }
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
        dirs.extend(self.home.iter().map(|home| home.join(".local/bin")));
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

/// Provider paths for this device, each `None` when it stays as it is. Only paths and the
/// execution switch: variables with keys never pass through here.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Paths {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_server: Option<AppServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_bin: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_sessions: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_sdk_bin: Option<PathBuf>,
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
    pub fn names(&self) -> Vec<&'static str> {
        [
            ("execution", self.execution.is_some()),
            ("app_server", self.app_server.is_some()),
            ("codex_home", self.codex_home.is_some()),
            ("codex_bin", self.codex_bin.is_some()),
            ("codex_sessions", self.codex_sessions.is_some()),
            ("cursor_sdk_bin", self.cursor_sdk_bin.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, set)| set.then_some(name))
        .collect()
    }

    /// `other`'s values for what this leaves unset.
    pub fn or(self, other: Paths) -> Paths {
        Paths {
            execution: self.execution.or(other.execution),
            app_server: self.app_server.or(other.app_server),
            codex_home: self.codex_home.or(other.codex_home),
            codex_bin: self.codex_bin.or(other.codex_bin),
            codex_sessions: self.codex_sessions.or(other.codex_sessions),
            cursor_sdk_bin: self.cursor_sdk_bin.or(other.cursor_sdk_bin),
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
                .filter(|on| *on != target.local.switches.execution),
            app_server: self
                .app_server
                .filter(|server| *server != target.local.switches.codex_app_server),
            codex_home: self.codex_home.filter(|home| {
                target.local.env.get(Provider::Codex).get(CODEX_HOME)
                    != Some(&home.to_string_lossy().into_owned())
            }),
            codex_bin: differs(self.codex_bin, Some(&target.local.codex_bin)),
            codex_sessions: differs(self.codex_sessions, target.triage.codex_sessions.as_ref()),
            cursor_sdk_bin: differs(
                self.cursor_sdk_bin,
                target.manager.as_ref().map(|m| &m.cursor_sdk_bin),
            ),
        }
    }

    pub fn apply(&self, target: Target) -> Result<()> {
        if let Some(on) = self.execution {
            target.local.switches.execution = on;
        }
        if let Some(server) = &self.app_server {
            target.local.switches.codex_app_server = server.clone();
        }
        if let Some(home) = &self.codex_home {
            target.local.env.apply(
                Provider::Codex,
                BTreeMap::from([(CODEX_HOME.into(), Some(home.to_string_lossy().into_owned()))]),
            )?;
        }
        if let Some(bin) = &self.codex_bin {
            target.local.codex_bin = bin.clone();
        }
        if let Some(sessions) = &self.codex_sessions {
            target.triage.codex_sessions = Some(sessions.clone());
        }
        if let Some(bin) = &self.cursor_sdk_bin {
            match target.manager {
                Some(manager) if manager.provider == ManagerKind::CursorSdk => {
                    manager.cursor_sdk_bin = bin.clone()
                }
                _ => bail!("the Cursor program is set while Cursor runs the bot"),
            }
        }
        Ok(())
    }
}

/// What this computer supplies for the uses that are on and miss a path. `codex` also counts
/// Codex as in use when it is about to be. Fails only when Codex is needed and has no folder.
pub fn missing(target: &Target, codex: bool, host: &Host) -> Result<Paths> {
    let local = &*target.local;
    let codex = codex
        || local.switches.execution
        || target
            .manager
            .as_ref()
            .is_some_and(|m| m.provider == ManagerKind::CodexSdk);
    let home_set = local.env.get(Provider::Codex).contains_key(CODEX_HOME);
    let mut paths = Paths::default();
    let server = &local.switches.codex_app_server;
    let mut spawn = server.mode == AppServerMode::Spawn;
    if codex {
        if server.mode == AppServerMode::System && server.socket.is_none() {
            let home = host.codex_home()?;
            match control_socket(&home) {
                Some(socket) => {
                    paths.app_server = Some(AppServer {
                        mode: AppServerMode::System,
                        socket: Some(socket),
                    })
                }
                None => {
                    spawn = true;
                    paths.app_server = Some(AppServer {
                        mode: AppServerMode::Spawn,
                        socket: None,
                    });
                    if !home_set {
                        paths.codex_home = Some(home);
                    }
                }
            }
        } else if spawn && !home_set {
            paths.codex_home = Some(host.codex_home()?);
        }
        if spawn && !runnable(&local.codex_bin, host.path.as_ref()) {
            paths.codex_bin = bare(&local.codex_bin).and_then(|name| host.program(name));
        }
    }
    if target.triage.codex_sessions.is_none() {
        if target.triage.scan_codex_sessions {
            match sessions_dir(&host.codex_home()?) {
                Some(sessions) => paths.codex_sessions = Some(sessions),
                None => bail!("this computer's Codex folder has no sessions folder yet"),
            }
        } else if codex {
            paths.codex_sessions = host.codex_home().ok().as_deref().and_then(sessions_dir);
        }
    }
    if let Some(manager) = target.manager.as_ref()
        && manager.provider == ManagerKind::CursorSdk
        && !runnable(&manager.cursor_sdk_bin, host.path.as_ref())
    {
        paths.cursor_sdk_bin = bare(&manager.cursor_sdk_bin).and_then(|name| host.program(name));
    }
    Ok(paths)
}

/// Fills what `config` and `env` miss for the uses they turn on. `init` runs it before the
/// first save, and a Node before it starts with a saved configuration.
pub fn complete(config: &mut Config, env: &mut ProviderEnv, host: &Host) -> Result<Paths> {
    let mut local = Local {
        switches: config.providers.clone(),
        codex_bin: config.codex_bin.clone(),
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
    config.codex_bin = local.codex_bin;
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
            tracing::warn!(%error, "provider paths are missing and could not be found");
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
            codex_bin: "codex".into(),
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
    fn codex_home_from_the_environment_comes_before_the_home_folder() -> Result<()> {
        let root = scratch("order");
        let home = root.join("home");
        let env_home = root.join("env-codex");
        std::fs::create_dir_all(home.join(".codex"))?;
        std::fs::create_dir_all(&env_home)?;
        let host = Host {
            codex_home: Some(env_home.clone()),
            home: Some(home.clone()),
            path: None,
        };
        assert_eq!(host.codex_home()?, env_home);
        let host = Host {
            codex_home: None,
            ..host
        };
        assert_eq!(host.codex_home()?, home.join(".codex"));
        let missing_env = Host {
            codex_home: Some(root.join("absent")),
            home: Some(home),
            path: None,
        };
        assert!(
            missing_env.codex_home().is_err(),
            "a CODEX_HOME that does not exist is an error, not a reason to look elsewhere"
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn without_a_codex_folder_turning_codex_on_fails_clearly() -> Result<()> {
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
        let error = missing(&target, true, &host).unwrap_err().to_string();
        assert!(error.contains("no Codex folder found"), "{error}");
        assert!(
            !error.contains(&root.to_string_lossy().into_owned()),
            "the error names no local path"
        );
        assert!(
            missing(&target, false, &host)?.is_empty(),
            "nothing is looked up while Codex is not in use"
        );
        assert!(missing(&target, true, &Host::default()).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn a_running_app_server_is_used_and_otherwise_the_node_starts_its_own() -> Result<()> {
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
        assert_eq!(
            found.app_server,
            Some(AppServer {
                mode: AppServerMode::Spawn,
                socket: None
            })
        );
        assert_eq!(found.codex_home.as_ref(), Some(&codex));
        assert_eq!(found.codex_sessions, Some(codex.join("sessions")));

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
        assert_eq!(
            found.app_server,
            Some(AppServer {
                mode: AppServerMode::System,
                socket: Some(socket)
            })
        );
        assert_eq!(
            found.codex_home, None,
            "the system App Server keeps its own folder"
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn what_startup_finds_is_saved_and_read_back() -> Result<()> {
        let root = scratch("startup");
        let codex = root.join(".codex");
        std::fs::create_dir_all(codex.join("sessions"))?;
        let dir = root.join("node");
        crate::config::private_dir(&dir)?;
        // Saved by a Node before discovery: execution on, nothing to run it with.
        let mut config =
            serde_json::to_value(Config::local("127.0.0.1:0".parse()?, vec![], false))?;
        config["providers"]["execution"] = true.into();
        std::fs::write(dir.join("config.json"), serde_json::to_vec(&config)?)?;
        let host = Host {
            codex_home: None,
            home: Some(root.clone()),
            path: None,
        };
        complete_saved(&dir, &host)?;
        let saved = Config::load(&dir)?;
        assert_eq!(saved.providers.codex_app_server.mode, AppServerMode::Spawn);
        assert_eq!(saved.triage.codex_sessions, Some(codex.join("sessions")));
        assert_eq!(
            ProviderEnv::load(&dir)?
                .get(crate::providers::Provider::Codex)
                .get(CODEX_HOME)
                .map(PathBuf::from),
            Some(codex.clone())
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
            provider: ManagerKind::CursorSdk,
            workspace: root.clone(),
            model: "auto".into(),
            cursor_sdk_bin: "cursor-sdk-bridge".into(),
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
        assert_eq!(found.cursor_sdk_bin, Some(bridge));
        manager.as_mut().unwrap().cursor_sdk_bin = root.join("chosen/cursor-sdk-bridge");
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
}
