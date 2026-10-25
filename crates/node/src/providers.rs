//! The AI providers this Node runs locally and the variables each provider's processes get. The
//! bot's provider and the execution switch live in `config.json`; the variables are model keys,
//! so they stay in one private file of the data directory and never reach events, logs or the
//! web, which only learns which names are set.
use crate::manager::ManagerKind;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const ENV_FILE: &str = "provider-env.json";
const MAX_VARIABLES: usize = 64;
const CURSOR_KEY: &str = "CURSOR_API_KEY";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Codex,
    Cursor,
}

/// The coordination Agent (the bot) or the execution layer that runs dispatched work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Agent,
    Execution,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Codex, Provider::Cursor];

    /// What the runtime can run on each provider: dispatched work runs only on the Codex
    /// App Server, so Cursor serves only as the bot.
    pub fn supports(self) -> &'static [Role] {
        match self {
            Provider::Codex => &[Role::Agent, Role::Execution],
            Provider::Cursor => &[Role::Agent],
        }
    }

    pub fn of(kind: ManagerKind) -> Self {
        match kind {
            ManagerKind::CodexSdk => Provider::Codex,
            ManagerKind::CursorSdk => Provider::Cursor,
        }
    }

    /// The bot runtime that runs on this provider.
    pub fn manager_kind(self) -> ManagerKind {
        match self {
            Provider::Codex => ManagerKind::CodexSdk,
            Provider::Cursor => ManagerKind::CursorSdk,
        }
    }
}

/// Which Codex App Server this Node uses: the one already running on this computer, so its
/// sessions are shared, or one the Node starts and stops itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppServerMode {
    #[default]
    System,
    Spawn,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppServer {
    pub mode: AppServerMode,
    /// The explicitly selected system App Server socket. Kept while the
    /// Node starts its own, so switching back restores it.
    pub socket: Option<PathBuf>,
}

/// What the providers do besides the bot, which `Config.manager` chooses. A provider is in
/// use while the bot runs on it or, for Codex, while it executes dispatched work.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Providers {
    /// Dispatched work runs on the Codex App Server.
    pub execution: bool,
    #[serde(default)]
    pub codex_app_server: AppServer,
}

impl Default for Providers {
    fn default() -> Self {
        Self {
            execution: false,
            codex_app_server: AppServer::default(),
        }
    }
}

impl Providers {
    pub fn validate(&self, codex_agent: bool) -> Result<()> {
        ensure!(
            !(self.execution || codex_agent)
                || self.codex_app_server.mode != AppServerMode::System
                || self.codex_app_server.socket.is_some(),
            "set an explicit App Server socket before enabling system execution"
        );
        if let Some(socket) = &self.codex_app_server.socket {
            let text = socket.to_string_lossy();
            ensure!(
                socket.is_absolute() && text.len() <= 1024 && !text.contains('\0'),
                "the App Server socket must be an absolute path"
            );
        }
        Ok(())
    }
}

/// The variables each provider's processes get on top of the Node's own environment. A
/// provider's variables never reach another provider's processes. No `Debug`: it holds keys.
#[derive(Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEnv {
    #[serde(default)]
    codex: BTreeMap<String, String>,
    #[serde(default)]
    cursor: BTreeMap<String, String>,
}

impl ProviderEnv {
    pub fn load(dir: &Path) -> Result<Self> {
        let env: Self = match std::fs::read(dir.join(ENV_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("invalid provider environment")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error.into()),
        };
        for provider in Provider::ALL {
            for (name, value) in env.get(provider) {
                validate(provider, name, value)?;
                crate::logging::add_private_path(std::path::Path::new(value));
            }
        }
        env.redact_in_logs();
        Ok(env)
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        crate::config::private_write(&dir.join(ENV_FILE), &serde_json::to_vec(self)?)?;
        self.redact_in_logs();
        Ok(())
    }

    pub fn get(&self, provider: Provider) -> &BTreeMap<String, String> {
        match provider {
            Provider::Codex => &self.codex,
            Provider::Cursor => &self.cursor,
        }
    }

    /// `Some` sets or replaces a variable, `None` removes it.
    pub fn apply(
        &mut self,
        provider: Provider,
        changes: BTreeMap<String, Option<String>>,
    ) -> Result<()> {
        let env = match provider {
            Provider::Codex => &mut self.codex,
            Provider::Cursor => &mut self.cursor,
        };
        for (name, value) in changes {
            match value {
                Some(value) => {
                    validate(provider, &name, &value)?;
                    env.insert(name, value);
                }
                None => {
                    env.remove(&name);
                }
            }
        }
        ensure!(
            env.len() <= MAX_VARIABLES,
            "at most {MAX_VARIABLES} variables per provider"
        );
        Ok(())
    }

    fn redact_in_logs(&self) {
        for value in self.codex.values().chain(self.cursor.values()) {
            crate::logging::add_secret(value);
        }
    }
}

fn validate(provider: Provider, name: &str, value: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 256
            && name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
            }),
        "variable names use letters, digits and underscores"
    );
    ensure!(
        !name.starts_with("VIBEFACE_")
            && !(provider == Provider::Cursor && name.starts_with("CURSOR_SDK_")),
        "{name} is set by Vibeface"
    );
    ensure!(
        !value.is_empty() && value.len() <= 4096 && !value.contains('\0'),
        "a variable value must be 1 to 4096 characters"
    );
    Ok(())
}

/// The Cursor key the bridge is started with and the rest of the Cursor variables. A key set
/// here wins over the one the Node process inherited.
pub fn cursor_launch(env: &BTreeMap<String, String>) -> (Option<String>, BTreeMap<String, String>) {
    let mut env = env.clone();
    let key = env
        .remove(CURSOR_KEY)
        .or_else(|| std::env::var(CURSOR_KEY).ok());
    (key, env)
}

/// What the Node's App Server is started with.
#[derive(Clone, PartialEq)]
pub struct CodexLaunch {
    pub bin: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// The App Server the Node talks to right now.
#[derive(Clone, PartialEq)]
pub enum CodexTarget {
    /// Codex does not execute work.
    Off,
    /// The App Server at the explicitly configured socket.
    System(PathBuf),
    Spawn(CodexLaunch),
}

impl CodexTarget {
    /// Which App Server keeps the threads: the explicitly selected system server, or the
    /// Node's own by the Codex home it stores them in. A thread or turn belongs to one of them.
    pub fn server(&self) -> String {
        match self {
            CodexTarget::Off => "off".into(),
            CodexTarget::System(_) => "system".into(),
            CodexTarget::Spawn(launch) => format!(
                "own:{}",
                launch.env.get("CODEX_HOME").map_or("", String::as_str)
            ),
        }
    }
}

/// Everything about the providers that only this machine knows.
#[derive(Clone)]
pub struct Local {
    pub switches: Providers,
    pub codex_bin: PathBuf,
    pub env: ProviderEnv,
}

impl Local {
    pub fn validate(&self, codex_agent: bool) -> Result<()> {
        self.switches.validate(codex_agent)?;
        if (self.switches.execution || codex_agent)
            && self.switches.codex_app_server.mode == AppServerMode::Spawn
        {
            ensure!(
                self.env
                    .codex
                    .get("CODEX_HOME")
                    .is_some_and(|home| std::path::Path::new(home).is_absolute()),
                "set an explicit absolute CODEX_HOME before starting a managed App Server"
            );
        }
        Ok(())
    }
    /// The variables reach only an App Server the Node starts; the system one keeps its own.
    pub fn codex_target(&self, codex_agent: bool) -> CodexTarget {
        let app_server = &self.switches.codex_app_server;
        match (self.switches.execution || codex_agent, app_server.mode) {
            (false, _) => CodexTarget::Off,
            (true, AppServerMode::System) => match &app_server.socket {
                Some(socket) => CodexTarget::System(socket.clone()),
                None => CodexTarget::Off,
            },
            (true, AppServerMode::Spawn) => CodexTarget::Spawn(CodexLaunch {
                bin: self.codex_bin.clone(),
                env: self.env.codex.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_belong_to_the_app_server_that_keeps_them() {
        let own = |home: Option<&str>| {
            CodexTarget::Spawn(CodexLaunch {
                bin: "codex".into(),
                env: home
                    .map(|home| ("CODEX_HOME".to_owned(), home.to_owned()))
                    .into_iter()
                    .collect(),
            })
            .server()
        };
        assert_eq!(
            CodexTarget::System("/tmp/first.sock".into()).server(),
            CodexTarget::System("/tmp/a.sock".into()).server()
        );
        assert_ne!(
            CodexTarget::System("/tmp/first.sock".into()).server(),
            own(None)
        );
        assert_ne!(own(None), own(Some("/tmp/home")));
    }

    #[test]
    fn variables_are_validated_and_stay_with_their_provider() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("vibeface-provider-env-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let mut env = ProviderEnv::load(&dir)?;
        env.apply(
            Provider::Codex,
            BTreeMap::from([("CPA_API_KEY".into(), Some("fixture-codex-value".into()))]),
        )?;
        env.apply(
            Provider::Cursor,
            BTreeMap::from([("CURSOR_API_KEY".into(), Some("fixture-cursor-value".into()))]),
        )?;
        for (provider, name) in [
            (Provider::Codex, "VIBEFACE_DATA_DIR"),
            (Provider::Cursor, "CURSOR_SDK_PORT"),
            (Provider::Codex, "1KEY"),
            (Provider::Codex, "A-B"),
        ] {
            let mut copy = env.clone();
            assert!(
                copy.apply(
                    provider,
                    BTreeMap::from([(name.into(), Some("value".into()))])
                )
                .is_err(),
                "{name}"
            );
        }
        env.save(&dir)?;
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.join(ENV_FILE))?.permissions().mode() & 0o777,
            0o600
        );
        let loaded = ProviderEnv::load(&dir)?;
        assert!(!loaded.get(Provider::Cursor).contains_key("CPA_API_KEY"));
        assert!(!loaded.get(Provider::Codex).contains_key("CURSOR_API_KEY"));
        let (key, rest) = cursor_launch(loaded.get(Provider::Cursor));
        assert_eq!(key.as_deref(), Some("fixture-cursor-value"));
        assert!(rest.is_empty());
        let mut removed = loaded.clone();
        removed.apply(
            Provider::Codex,
            BTreeMap::from([("CPA_API_KEY".into(), None)]),
        )?;
        assert!(removed.get(Provider::Codex).is_empty());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn the_execution_switch_decides_whether_codex_runs_work() {
        let mut local = Local {
            switches: Providers::default(),
            codex_bin: "codex".into(),
            env: ProviderEnv::default(),
        };
        assert!(!local.switches.execution && local.switches.validate(false).is_ok());
        assert!(local.codex_target(false) == CodexTarget::Off);
        assert!(local.validate(true).is_err());
        local.switches.execution = true;
        assert!(local.validate(false).is_err());
        local.switches.codex_app_server.socket = Some("/configured/app-server.sock".into());
        assert!(local.validate(false).is_ok());
        assert!(matches!(local.codex_target(false), CodexTarget::System(_)));
        local.switches.execution = false;
        assert!(local.validate(true).is_ok());
        assert!(matches!(local.codex_target(true), CodexTarget::System(_)));
        assert!(local.codex_target(false) == CodexTarget::Off);
        assert!(
            serde_json::from_value::<Providers>(serde_json::json!({
                "execution":true,"codex":{"enabled":true,"roles":["agent"]},
            }))
            .is_err(),
            "per-provider switches are gone"
        );
    }

    #[test]
    fn codex_execution_requires_an_explicit_socket_or_spawn_mode() -> Result<()> {
        let saved: Providers = serde_json::from_value(serde_json::json!({"execution":true}))?;
        let mut env = ProviderEnv::default();
        env.apply(
            Provider::Codex,
            BTreeMap::from([("CPA_API_KEY".into(), Some("fixture-value".into()))]),
        )?;
        let mut local = Local {
            switches: saved,
            codex_bin: "codex".into(),
            env,
        };
        assert!(local.switches.validate(false).is_err());
        assert!(local.codex_target(false) == CodexTarget::Off);
        local.switches.codex_app_server = AppServer {
            mode: AppServerMode::Spawn,
            socket: Some("/tmp/kept.sock".into()),
        };
        let CodexTarget::Spawn(launch) = local.codex_target(false) else {
            panic!("spawn mode starts the Node's own App Server");
        };
        assert_eq!(launch.env["CPA_API_KEY"], "fixture-value");
        local.switches.codex_app_server.mode = AppServerMode::System;
        assert!(local.codex_target(false) == CodexTarget::System("/tmp/kept.sock".into()));
        local.switches.codex_app_server.socket = Some("relative.sock".into());
        assert!(local.switches.validate(false).is_err());
        Ok(())
    }
}
