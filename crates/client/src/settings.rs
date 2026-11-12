use crate::outbox::{Operation, OperationState};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsKind {
    Node,
    Provider,
    Triage,
    Relay,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsField {
    pub name: String,
    pub value: String,
    pub secret: bool,
}
#[derive(Clone, Debug)]
pub struct SettingsReview {
    pub operation_id: String,
    pub network_id: String,
    pub client_id: String,
    pub target_node: String,
    pub kind: SettingsKind,
    pub fields: Vec<SettingsField>,
    pub has_secret_changes: bool,
    pub dependent_ids: Vec<String>,
    pub(crate) fingerprint: String,
}
pub(crate) fn kind(request: &Value) -> Option<SettingsKind> {
    match request["method"].as_str() {
        Some("configure_node") => Some(SettingsKind::Node),
        Some("configure_provider") => Some(SettingsKind::Provider),
        Some("configure_triage") => Some(SettingsKind::Triage),
        Some("configure_relay") => Some(SettingsKind::Relay),
        _ => None,
    }
}
pub(crate) fn read_method(kind: SettingsKind) -> &'static str {
    match kind {
        SettingsKind::Node | SettingsKind::Provider => "node_settings",
        SettingsKind::Triage => "triage_settings",
        SettingsKind::Relay => "relay_settings",
    }
}
pub(crate) fn goal_matches(kind: SettingsKind, wanted: &Value, current: &Value) -> bool {
    match kind {
        SettingsKind::Node => wanted == &current["settings"],
        // Provider setup can discover or replace private variables which its public snapshot never reveals.
        SettingsKind::Provider => false,
        SettingsKind::Triage => {
            if wanted
                .get("webhook_secret")
                .is_some_and(|secret| !secret.is_null())
            {
                return false;
            }
            if wanted.get("webhook_secret").is_some() && current["webhook_secret_set"] != false {
                return false;
            }
            [
                "default_outlet",
                "projects",
                "webhook_url",
                "scan_codex_sessions",
                "codex_sessions",
            ]
            .iter()
            .all(|field| wanted[*field] == current[*field])
        }
        SettingsKind::Relay => {
            if wanted["volunteer"] != current["volunteer"] {
                return false;
            }
            if wanted["volunteer"] == false {
                return true;
            }
            ["listen", "url", "rate", "max_clients", "max_clients_per_ip"]
                .iter()
                .all(|field| wanted[*field].is_null() || wanted[*field] == current[*field])
        }
    }
}
pub(crate) fn review(operation: &Operation, dependent_ids: Vec<String>) -> Result<SettingsReview> {
    ensure!(
        operation.snapshot.state == OperationState::Unknown,
        "only an unknown settings operation needs review"
    );
    let request: Value = serde_json::from_slice(&operation.bytes)?;
    let kind = kind(&request).context(
        "this operation needs its authoritative receipt; it cannot be closed as settings",
    )?;
    let wanted = &request["params"];
    let names: &[&str] = match kind {
        SettingsKind::Node => &["roots", "manager"],
        SettingsKind::Provider => &[
            "provider",
            "bot",
            "execution",
            "execution_model",
            "bin",
            "app_server",
            "connect_local",
        ],
        SettingsKind::Triage => &[
            "default_outlet",
            "projects",
            "webhook_url",
            "scan_codex_sessions",
            "codex_sessions",
        ],
        SettingsKind::Relay => &[
            "volunteer",
            "listen",
            "url",
            "rate",
            "max_clients",
            "max_clients_per_ip",
        ],
    };
    let mut fields = names
        .iter()
        .filter_map(|name| {
            wanted.get(*name).map(|value| SettingsField {
                name: (*name).into(),
                value: if matches!(*name, "webhook_url" | "url") && !value.is_null() {
                    "configured URL; value hidden".into()
                } else {
                    value.to_string()
                },
                secret: matches!(*name, "webhook_url" | "url"),
            })
        })
        .collect::<Vec<_>>();
    let mut has_secret_changes = false;
    if let Some(env) = wanted["env"].as_object() {
        has_secret_changes = !env.is_empty();
        for (name, value) in env {
            let valid = name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            fields.push(SettingsField {
                name: format!(
                    "env.{}",
                    if valid {
                        name.as_str()
                    } else {
                        "[invalid name]"
                    }
                ),
                value: if value.is_null() {
                    "clear"
                } else {
                    "set; value hidden"
                }
                .into(),
                secret: true,
            });
        }
    }
    if let Some(endpoint) = wanted.get("opencode_endpoint") {
        has_secret_changes |= endpoint.is_null();
        fields.push(SettingsField {
            name: "opencode_endpoint".into(),
            value: if endpoint.is_null() {
                "remove"
            } else {
                "configure; endpoint and credentials hidden"
            }
            .into(),
            secret: true,
        });
        if let Some(secret) = endpoint.get("api_key") {
            has_secret_changes = true;
            fields.push(SettingsField {
                name: "opencode_endpoint.api_key".into(),
                value: if secret.is_null() {
                    "clear"
                } else {
                    "set; value hidden"
                }
                .into(),
                secret: true,
            });
        }
    }
    if wanted["connect_local"] == true
        || kind == SettingsKind::Provider && (wanted["bot"] == true || wanted["execution"] == true)
    {
        has_secret_changes = true;
    }
    if let Some(secret) = wanted.get("webhook_secret") {
        has_secret_changes = true;
        fields.push(SettingsField {
            name: "webhook_secret".into(),
            value: if secret.is_null() {
                "clear"
            } else {
                "set; value hidden"
            }
            .into(),
            secret: true,
        });
    }
    Ok(SettingsReview {
        operation_id: operation.snapshot.id.clone(),
        network_id: operation.snapshot.network_id.clone(),
        client_id: operation.snapshot.client_id.clone(),
        target_node: operation.snapshot.target_node.clone(),
        kind,
        fields,
        has_secret_changes,
        dependent_ids,
        fingerprint: hex::encode(Sha256::digest(&operation.bytes)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn positive_readback_requires_the_full_public_goal_and_never_guesses_secret_values() {
        let wanted = json!({"default_outlet":"record","projects":[],"webhook_url":null,"scan_codex_sessions":false,"codex_sessions":null,"webhook_secret":"synthetic-secret"});
        let mut current = json!({"default_outlet":"record","projects":[],"webhook_url":null,"scan_codex_sessions":false,"codex_sessions":null,"webhook_secret_set":true});
        assert!(!goal_matches(SettingsKind::Triage, &wanted, &current));
        let mut clear = wanted;
        clear["webhook_secret"] = Value::Null;
        assert!(!goal_matches(SettingsKind::Triage, &clear, &current));
        current["webhook_secret_set"] = json!(false);
        assert!(goal_matches(SettingsKind::Triage, &clear, &current));
        current["scan_codex_sessions"] = json!(true);
        assert!(!goal_matches(SettingsKind::Triage, &clear, &current));
        assert!(!goal_matches(
            SettingsKind::Provider,
            &json!({"env":{"KEY":"x"}}),
            &json!({"env":{"KEY":{"set":true}}})
        ));
    }
    #[test]
    fn local_settings_closure_never_classifies_cumulative_or_signed_actions_as_settings() {
        for method in [
            "test_webhook",
            "create_decision_token",
            "revoke_decision_token",
            "delivery",
            "rpc",
            "recover",
            "approve",
            "grant",
            "revoke",
        ] {
            assert_eq!(
                kind(&json!({"method":method,"params":{"method":"configure_node"}})),
                None
            );
        }
        assert_eq!(
            kind(&json!({"method":"configure_node"})),
            Some(SettingsKind::Node)
        );
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;
    use crate::outbox::Reconciliation;
    use serde_json::json;

    #[test]
    fn endpoint_review_hides_urls_and_keys_and_reports_explicit_clear() -> Result<()> {
        for endpoint in [
            Value::Null,
            json!({"protocol":"openai","base_url":"https://synthetic-token@fixture.invalid","models":["fixture"],"api_key":"synthetic-secret"}),
            json!({"protocol":"openai","base_url":"https://fixture.invalid","models":[],"api_key":null}),
        ] {
            let operation = Operation {
                snapshot: crate::OperationSnapshot {
                    id: "original".into(),
                    network_id: "network".into(),
                    client_id: "client".into(),
                    target_node: "node".into(),
                    state: OperationState::Unknown,
                    sequence: None,
                    result: None,
                },
                lane: "provider-settings".into(),
                expected_revision: None,
                bytes: serde_json::to_vec(
                    &json!({"id":"original","method":"configure_provider","params":{"provider":"opencode","opencode_endpoint":endpoint}}),
                )?,
                reconciliation: Reconciliation::Manual,
                attachments: vec![],
                upload_scope: None,
                prerequisites: vec![],
            };
            let result = review(&operation, vec![])?;
            assert!(result.has_secret_changes);
            assert_eq!(result.operation_id, "original");
            let display = format!("{result:?}");
            assert!(!display.contains("synthetic-secret"));
            assert!(!display.contains("synthetic-token"));
            assert!(!display.contains("fixture.invalid"));
            assert!(
                result
                    .fields
                    .iter()
                    .any(|field| field.name == "opencode_endpoint" && field.secret)
            );
        }
        Ok(())
    }
}
