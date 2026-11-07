//! Finds the paths this computer's providers need and writes them into the Node's own
//! configuration once, where Settings shows them and the owner can change them. Nothing is
//! guessed while running: a value is filled only for an enabled use or explicit local connection.
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

mod environment;

pub(crate) struct LocalConnect {
    pub provider: Provider,
    pub explicit_env_names: BTreeSet<String>,
}

impl LocalConnect {
    fn names(&self, local: &Local, codex_config: Option<&str>) -> Result<Vec<String>> {
        if !local.launches(self.provider) {
            return Ok(vec![]);
        }
        let keys = match (self.provider, codex_config) {
            (Provider::Codex, Some(source)) => crate::manager::provider_keys_from_config(source),
            _ => self.provider.key_variables(local.env.get(self.provider)),
        };
        Ok(keys
            .map_err(|_| LocalSourceError)?
            .into_iter()
            .filter(|name| {
                !local.env.get(self.provider).contains_key(name)
                    && !self.explicit_env_names.contains(name)
            })
            .collect())
    }
}

#[derive(Debug)]
pub(crate) struct LocalSourceError;

impl std::fmt::Display for LocalSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "could not read local AI service settings; retry or configure the service manually",
        )
    }
}

impl std::error::Error for LocalSourceError {}

pub(crate) async fn complete_local(
    local: &mut Local,
    connect: &LocalConnect,
    codex_config: Option<&str>,
    host: &Host,
) -> Result<bool> {
    if !local.launches(connect.provider) {
        return Ok(false);
    }
    let names = connect.names(local, codex_config)?;
    let values = environment::missing(&names, host).await?;
    if values.is_empty() {
        return Ok(false);
    }
    for value in values.values() {
        crate::logging::add_credential(value);
    }
    let mut next = local.env.clone();
    next.apply(
        connect.provider,
        values
            .into_iter()
            .map(|(name, value)| (name, Some(value)))
            .collect(),
    )
    .map_err(|_| LocalSourceError)?;
    local.env = next;
    Ok(true)
}

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
    pub nvm_dir: Option<PathBuf>,
    pub path: Option<OsString>,
    #[cfg(test)]
    pub inherited_env: BTreeMap<String, String>,
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
            nvm_dir: dir("NVM_DIR"),
            path: std::env::var_os("PATH"),
            #[cfg(test)]
            inherited_env: BTreeMap::new(),
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
            dirs.extend(self.nvm_bins());
        }
        dirs.into_iter()
            .map(|dir| dir.join(name))
            .find(|path| executable_file(path))
    }

    fn nvm_bins(&self) -> Vec<PathBuf> {
        let Some(root) = self
            .nvm_dir
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".nvm")))
        else {
            return vec![];
        };
        if !root.is_absolute() {
            return vec![];
        }
        let mut versions: Vec<_> = std::fs::read_dir(root.join("versions/node"))
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name();
                let version = semver::Version::parse(name.to_str()?.strip_prefix('v')?).ok()?;
                let bin = entry.path().join("bin");
                executable_file(&bin.join("node")).then_some((version, bin))
            })
            .collect();
        versions.sort_by(|(a, path_a), (b, path_b)| b.cmp(a).then(path_a.cmp(path_b)));
        versions.into_iter().map(|(_, path)| path).collect()
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

pub(crate) fn executable_file(path: &Path) -> bool {
    vibeface_bridge::platform::privatefs::is_executable(path)
}

/// Whether `program` runs as configured: a path that is an executable file, or a bare name
/// found on the Node's `PATH`.
pub fn runnable(program: &Path, path: Option<&OsString>) -> bool {
    use vibeface_bridge::platform::program;
    if program.components().count() != 1 || program.is_absolute() {
        return program::runnable(program);
    }
    path.is_some_and(|paths| {
        std::env::split_paths(paths).any(|dir| {
            program::candidates(&dir.join(program))
                .iter()
                .any(|candidate| program::runnable(candidate))
        })
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

/// What this computer supplies for enabled uses or an explicitly requested service.
/// External session scanning needs an explicit root.
pub fn missing(target: &Target, requested: Option<Provider>, host: &Host) -> Result<Paths> {
    let local = &*target.local;
    let codex = requested == Some(Provider::Codex)
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
    if target.triage.codex_sessions.is_none() && target.triage.scan_codex_sessions {
        bail!("set an explicit Codex sessions directory before scanning external sessions");
    }
    let bot = target.manager.as_ref().map(|manager| manager.provider);
    for provider in Provider::ALL {
        let launches = match provider.on_app_server() {
            true => codex && spawn,
            false => {
                requested == Some(provider)
                    || bot == Some(provider)
                    || local.switches.execution.contains(&provider)
            }
        };
        let bin = local.bin(provider);
        let provider_host = Host {
            path: local
                .env
                .get(provider)
                .get("PATH")
                .map(OsString::from)
                .or(host.path.clone()),
            ..host.clone()
        };
        if launches
            && !runnable(&bin, provider_host.path.as_ref())
            && let Some(found) = bare(&bin).and_then(|name| provider_host.program(name))
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
        None,
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

#[cfg(all(test, unix))]
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

    fn environment_fixture(name: &str, script: &str) -> (PathBuf, Host, environment::Shell) {
        use std::os::unix::fs::PermissionsExt;
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("local-environment-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let path = dir.join("zsh");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let host = Host {
            home: Some(dir.clone()),
            path: Some("/usr/bin:/bin".into()),
            ..Default::default()
        };
        (
            dir,
            host,
            environment::Shell {
                path,
                user: "fixture".into(),
            },
        )
    }

    #[tokio::test]
    async fn local_environment_uses_only_required_names_and_inherited_values_win() -> Result<()> {
        let (dir, host, shell) = environment_fixture(
            "priority",
            r#"
printf 'startup noise\000ignored\n'
. "$HOME/fixture-env"
exec /bin/sh -c "$2"
"#,
        );
        std::fs::write(
            dir.join("fixture-env"),
            "export FIRST_KEY='synthetic-shell-first'\nexport SECOND_KEY='synthetic-shell-second'\nexport OTHER_PROVIDER_KEY='synthetic-other'\n",
        )?;
        let names = vec!["FIRST_KEY".into(), "SECOND_KEY".into(), "ABSENT_KEY".into()];
        let inherited = BTreeMap::from([
            ("FIRST_KEY".into(), "synthetic-inherited".into()),
            ("OTHER_PROVIDER_KEY".into(), "synthetic-other".into()),
        ]);
        let values = environment::missing_from(
            &names,
            &host,
            &inherited,
            Some(&shell),
            std::time::Duration::from_secs(2),
        )
        .await?;
        assert!(
            values
                .get("FIRST_KEY")
                .is_some_and(|value| value == "synthetic-inherited")
        );
        assert!(
            values
                .get("SECOND_KEY")
                .is_some_and(|value| value == "synthetic-shell-second")
        );
        assert_eq!(values.len(), 2);
        let missing_shell = environment::Shell {
            path: dir.join("missing/zsh"),
            user: "fixture".into(),
        };
        assert!(
            environment::missing_from(
                &names[..1],
                &host,
                &inherited,
                Some(&missing_shell),
                std::time::Duration::from_secs(2)
            )
            .await
            .is_ok(),
            "a complete inherited source never runs a shell"
        );
        assert!(
            environment::missing_from(
                &names,
                &host,
                &inherited,
                Some(&missing_shell),
                std::time::Duration::from_secs(2)
            )
            .await
            .err()
            .is_some_and(|error| error.is::<LocalSourceError>()),
            "a missing supported shell is a source failure when there is a gap"
        );
        assert!(
            environment::missing_from(
                &names,
                &host,
                &BTreeMap::new(),
                None,
                std::time::Duration::from_secs(2)
            )
            .await?
            .is_empty()
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn local_environment_failures_are_safe_bounded_and_stop_shell_helpers() -> Result<()> {
        for (case, script) in [
            ("frame", "printf 'synthetic-secret-output'; exit 0"),
            ("exit", "printf 'synthetic-secret-output' >&2; exit 7"),
            ("output", "head -c 400000 /dev/zero"),
            (
                "value",
                "export FIXTURE_KEY=$(head -c 4097 /dev/zero | tr '\\000' x); exec /bin/sh -c \"$2\"",
            ),
            (
                "timeout",
                "(sleep 0.4; touch \"$HOME/helper-survived\") &\nwait",
            ),
        ] {
            let (dir, host, shell) = environment_fixture(case, script);
            let error = environment::missing_from(
                &["FIXTURE_KEY".into()],
                &host,
                &BTreeMap::new(),
                Some(&shell),
                std::time::Duration::from_millis(150),
            )
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected a safe source failure"))?;
            assert!(error.is::<LocalSourceError>());
            assert!(!format!("{error:#}").contains("synthetic-secret-output"));
            if case == "timeout" {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                assert!(
                    !dir.join("helper-survived").exists(),
                    "the entire startup process group ends"
                );
            }
            std::fs::remove_dir_all(dir)?;
        }
        let (dir, host, shell) = environment_fixture("invalid-name", "touch \"$HOME/ran\"");
        assert!(
            environment::missing_from(
                &["KEY;touch bad".into()],
                &host,
                &BTreeMap::new(),
                Some(&shell),
                std::time::Duration::from_secs(1)
            )
            .await
            .is_err()
        );
        assert!(!dir.join("ran").exists());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn local_completion_keeps_private_values_and_explicit_removals_per_provider() -> Result<()> {
        let (dir, _, _) = environment_fixture("private", "exit 1");
        let roots = crate::providers::ProviderRoots::new(&dir);
        std::fs::create_dir_all(&roots.codex_home)?;
        std::fs::write(
            roots.codex_home.join("config.toml"),
            "model_provider='selected'\n[model_providers.selected]\nenv_key='PRIVATE_KEY'\nenv_http_headers={a='REMOVED_KEY',b='MISSING_KEY'}\n[model_providers.other]\nenv_key='OTHER_KEY'\n",
        )?;
        let mut local = local();
        roots.apply(&mut local)?;
        local.env.apply(
            Provider::Codex,
            BTreeMap::from([("PRIVATE_KEY".into(), Some("synthetic-private".into()))]),
        )?;
        let connect = LocalConnect {
            provider: Provider::Codex,
            explicit_env_names: ["REMOVED_KEY".into()].into(),
        };
        assert_eq!(connect.names(&local, None)?, vec!["MISSING_KEY"]);
        let cursor = LocalConnect {
            provider: Provider::Cursor,
            explicit_env_names: BTreeSet::new(),
        };
        assert_eq!(cursor.names(&local, None)?, vec!["CURSOR_API_KEY"]);
        local.env.apply(
            Provider::Cursor,
            BTreeMap::from([("CURSOR_API_KEY".into(), Some("synthetic-private".into()))]),
        )?;
        assert!(cursor.names(&local, None)?.is_empty());
        local.switches.codex_app_server.mode = AppServerMode::System;
        assert!(connect.names(&local, None)?.is_empty());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn requested_local_service_is_discovered_without_enabling_a_use() -> Result<()> {
        let (dir, host, _) = environment_fixture("disabled", "exit 1");
        let bridge = dir.join(".local/bin/cursor-sdk-bridge");
        program(&bridge);
        let (mut local, mut manager, mut triage) = (local(), None, TriageConfig::default());
        let found = missing(
            &Target {
                local: &mut local,
                manager: &mut manager,
                triage: &mut triage,
            },
            Some(Provider::Cursor),
            &host,
        )?;
        assert_eq!(found.bins.get(&Provider::Cursor), Some(&bridge));
        assert!(found.execution.is_empty());
        assert!(manager.is_none() && local.switches.execution.is_empty());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn settings_show_home_paths_from_tilde_and_read_typed_tilde_as_home() {
        let host = Host {
            codex_home: None,
            home: Some("/Users/a".into()),
            path: None,
            ..Default::default()
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
            ..Default::default()
        };
        let (mut local, mut manager, mut triage) = (local(), None, TriageConfig::default());
        let target = Target {
            local: &mut local,
            manager: &mut manager,
            triage: &mut triage,
        };
        assert!(missing(&target, Some(Provider::Codex), &host)?.is_empty());
        assert!(
            missing(&target, None, &host)?.is_empty(),
            "nothing is looked up while Codex is not in use"
        );
        assert!(missing(&target, Some(Provider::Codex), &Host::default())?.is_empty());
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
            ..Default::default()
        };
        let (mut local, mut manager, mut triage) = (local(), None, TriageConfig::default());
        let found = missing(
            &Target {
                local: &mut local,
                manager: &mut manager,
                triage: &mut triage,
            },
            Some(Provider::Codex),
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
            Some(Provider::Codex),
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
            ..Default::default()
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
            ..Default::default()
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
            None,
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
                None,
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
            ..Default::default()
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

    #[test]
    fn codex_nvm_discovery_uses_semver_and_keeps_the_wrapper_path() -> Result<()> {
        let root = scratch("nvm");
        let versions = root.join(".nvm/versions/node");
        for version in ["v9.0.0", "v20.9.0", "v20.10.0", "v99.0.0", "invalid"] {
            let install = versions.join(version);
            let wrapper = install.join("lib/node_modules/@openai/codex/bin/codex.js");
            program(&wrapper);
            std::fs::create_dir_all(install.join("bin"))?;
            std::os::unix::fs::symlink(
                "../lib/node_modules/@openai/codex/bin/codex.js",
                install.join("bin/codex"),
            )?;
            if version != "v99.0.0" {
                program(&install.join("bin/node"));
            } else {
                std::fs::write(install.join("bin/node"), "not executable")?;
            }
        }
        let path_bin = root.join("path/codex");
        let mut host = Host {
            home: Some(root.clone()),
            path: Some(root.join("path").into_os_string()),
            ..Default::default()
        };
        let selected = versions.join("v20.10.0/bin/codex");
        assert_eq!(host.program("codex"), Some(selected.clone()));
        assert_ne!(selected.canonicalize()?, selected);
        program(&path_bin);
        assert_eq!(host.program("codex"), Some(path_bin));
        host.path = None;
        assert_eq!(host.program("codex"), Some(selected));
        let custom = root.join("custom-nvm");
        std::fs::rename(root.join(".nvm"), &custom)?;
        host.nvm_dir = Some(custom.clone());
        host.home = None;
        assert_eq!(
            host.program("codex"),
            Some(custom.join("versions/node/v20.10.0/bin/codex"))
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
