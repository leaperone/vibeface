//! Input and bookkeeping shared by management turns. Adapters own the native send and read-back.
use super::{
    MAX_PROMPT, ManagerConfig, ManagerError, ManagerFailure, StartEvidence, bot_instructions,
};
use crate::providers::Provider;
use anyhow::{Result, ensure};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Mutex, PoisonError},
};
use tokio::sync::watch;
use vibeface_bridge::{
    agent::{redact_progress, redact_text},
    failure::{ExecFailure, FailureKind, ProviderStage, ProviderTrace},
};

pub(crate) struct TurnExecutionContext<'a> {
    pub(crate) config: &'a ManagerConfig,
    pub(crate) prompt: &'a str,
    pub(crate) previous: Option<&'a str>,
}

impl<'a> TurnExecutionContext<'a> {
    pub(super) fn new(
        config: &'a ManagerConfig,
        prompt: &'a str,
        previous: Option<&'a str>,
        cancel: &watch::Receiver<bool>,
    ) -> Result<Self> {
        (|| -> Result<()> {
            ensure!(config.timeout_secs > 0, "manager timeout must be positive");
            ensure!(
                config.workspace.is_absolute() && config.workspace.is_dir(),
                "manager workspace must be an existing absolute directory"
            );
            ensure!(
                !prompt.trim().is_empty() && prompt.len() <= MAX_PROMPT,
                "management prompt must be nonempty and at most 65536 bytes"
            );
            if let Some(id) = previous {
                vibeface_protocol::validate_id(id).map_err(anyhow::Error::msg)?;
            }
            config
                .sdk
                .provider()
                .validate_reasoning_effort(config.reasoning_effort.as_deref())
        })()
        .map_err(ManagerError::not_started)?;
        if *cancel.borrow() {
            return Err(ManagerError {
                stage: ProviderStage::Validate,
                outcome: ManagerFailure::Cancelled,
                start: StartEvidence::NotStarted,
                session_id: previous.map(str::to_owned),
                failure: ExecFailure::new(
                    FailureKind::Unknown,
                    "management cancelled before SDK launch",
                ),
            }
            .into());
        }
        Ok(Self {
            config,
            prompt,
            previous,
        })
    }

    pub(crate) fn trace(&self, initial: ProviderStage) -> TurnReport {
        let secrets = secret_values(self.config);
        let provider = self.config.sdk.provider();
        TurnReport {
            stage: ProviderTrace::new(provider.name(), initial),
            provider,
            start: Mutex::new(StartEvidence::NotStarted),
            secrets,
        }
    }

    pub(crate) fn tool_env(&self) -> BTreeMap<String, String> {
        let mut env = self.config.tool_env.clone();
        if let Some(token) = &self.config.tool_token {
            env.insert("VIBEFACE_TOOL_TOKEN".into(), token.clone());
        }
        env
    }

    pub(crate) fn child_env(&self) -> BTreeMap<String, String> {
        let mut env = self.provider_env().clone();
        env.extend(self.tool_env());
        env
    }

    pub(crate) fn provider_env(&self) -> &BTreeMap<String, String> {
        &self.config.provider_env
    }

    pub(crate) fn instructions(&self) -> String {
        bot_instructions(
            self.config.tool_cli.as_deref(),
            &self.config.custom_tools,
            &self.config.instructions,
        )
    }

    /// The adapter has checked whether the previous session is usable.
    pub(crate) fn prompt_for_session(&self, resumed: bool) -> &'a str {
        if !resumed && self.previous.is_some() {
            return self
                .config
                .turn_binding
                .as_ref()
                .and_then(|turn| turn.fresh_prompt.as_deref())
                .unwrap_or(self.prompt);
        }
        self.prompt
    }

    pub(crate) async fn record_native_session(
        &self,
        session: &str,
        turn_id: Option<&str>,
    ) -> Result<()> {
        if let Some(turn) = &self.config.turn_binding {
            (turn.record)(session.to_owned(), turn_id.map(str::to_owned)).await?;
        }
        Ok(())
    }

    pub(crate) async fn notify_started(&self, turn_id: &str) -> Result<()> {
        if let Some(started) = self
            .config
            .turn_binding
            .as_ref()
            .and_then(|turn| turn.started.as_ref())
        {
            started(turn_id.to_owned()).await?;
        }
        Ok(())
    }

    pub(crate) async fn record_started(&self, session: &str, turn_id: &str) -> Result<()> {
        self.record_native_session(session, Some(turn_id)).await?;
        self.notify_started(turn_id).await
    }

    pub(super) fn preflight_failure(
        &self,
        stage: ProviderStage,
        kind: FailureKind,
        detail: &str,
    ) -> ManagerError {
        ManagerError {
            stage,
            outcome: ManagerFailure::Failed,
            start: StartEvidence::NotStarted,
            session_id: self.previous.map(str::to_owned),
            failure: ExecFailure::new(kind, detail),
        }
    }
}

/// A provider advances the stage and records evidence only at its protocol's own boundaries.
pub(crate) struct TurnReport {
    stage: ProviderTrace,
    provider: Provider,
    pub(crate) start: Mutex<StartEvidence>,
    secrets: Vec<String>,
}

impl TurnReport {
    pub(crate) fn enter(&self, stage: ProviderStage) {
        self.stage.enter(stage);
    }

    pub(crate) fn set_start(&self, evidence: StartEvidence) {
        *self.start.lock().unwrap_or_else(PoisonError::into_inner) = evidence;
    }

    pub(crate) fn redact_progress(&self, update: Value) -> Value {
        redact_progress(update, &self.secrets)
    }

    pub(crate) fn redact_text(&self, text: String) -> String {
        redact_text(text, &self.secrets)
    }

    pub(crate) fn error(
        &self,
        outcome: ManagerFailure,
        session: Option<&str>,
        failure: ExecFailure,
    ) -> anyhow::Error {
        ManagerError {
            stage: self.stage.current(),
            outcome,
            start: *self.start.lock().unwrap_or_else(PoisonError::into_inner),
            session_id: session.map(str::to_owned),
            failure: ExecFailure {
                detail: self.redact_text(failure.detail),
                ..failure
            },
        }
        .into()
    }

    pub(crate) fn stopped(&self, session: Option<&str>, detail: &str) -> anyhow::Error {
        self.error(
            ManagerFailure::Cancelled,
            session,
            ExecFailure::new(FailureKind::Unknown, detail),
        )
    }

    /// An interruption can come from the owner in their own program on a shared server.
    pub(crate) fn interrupted(
        &self,
        cancel: &watch::Receiver<bool>,
        session: Option<&str>,
    ) -> anyhow::Error {
        let provider = self.provider.label();
        let (outcome, detail) = if *cancel.borrow() {
            (
                ManagerFailure::Cancelled,
                format!("{provider} turn was interrupted"),
            )
        } else {
            (
                ManagerFailure::Failed,
                format!("{provider} interrupted the turn without being asked to stop"),
            )
        };
        self.error(
            outcome,
            session,
            ExecFailure::new(FailureKind::Unknown, detail),
        )
    }
}

fn secret_values(config: &ManagerConfig) -> Vec<String> {
    let mut values: Vec<_> = config
        .tool_env
        .values()
        .chain(&config.tool_token)
        .chain(config.provider_env.values())
        .filter(|value| value.len() >= 8)
        .cloned()
        .collect();
    values.extend(
        std::env::vars()
            .filter(|(key, value)| value.len() >= 8 && crate::logging::credential_name(key))
            .map(|(_, value)| value),
    );
    if let Ok(keys) = config.sdk.provider().key_variables(&config.provider_env) {
        values.extend(
            keys.iter()
                .filter_map(|key| std::env::var(key).ok())
                .filter(|value| !value.is_empty()),
        );
    }
    values
}
