//! Every App Server method this client knows, from `schema/app-server-methods.json`. The table
//! says how a request may be used, how a notification enters the observed state and how a server
//! request is answered. A Codex upgrade that adds a method fails the coverage test until it is
//! classified here and in `docs/codex-app-server.md`.
use serde_json::{Value, json};

/// How a client request may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// No side effect; retrying is safe and a lost reply is not an unknown result.
    Read,
    /// Has a side effect; a lost reply is an unknown result that must be reconciled.
    Write,
    /// Never offered to the browser: file system, arbitrary commands, credential flows.
    Hidden,
}

/// The part of the observed state a notification updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Thread,
    Turn,
    Item,
    Plan,
    Reasoning,
    Usage,
    RateLimits,
    Account,
    Skills,
    Apps,
    Mcp,
    Hook,
    Review,
    Model,
    Warning,
    Search,
    Realtime,
    Platform,
    Import,
    Request,
    /// Command output, diffs, file changes and process output: kept, never shown.
    Output,
}

/// The deterministic answer to a server request on a thread this Node owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// The thread already runs with full access, so accepting widens nothing.
    Accept,
    /// Nobody is present to answer.
    Cancel,
    /// This client offers no dynamic tools.
    Fail,
    /// This client holds no credentials.
    Refuse,
}

use Access::*;
use Category::*;

pub const CLIENT_REQUESTS: &[(&str, Access)] = &[
    ("initialize", Hidden),
    ("thread/start", Write),
    ("thread/resume", Write),
    ("thread/fork", Write),
    ("thread/archive", Write),
    ("thread/delete", Write),
    ("thread/unsubscribe", Write),
    ("thread/name/set", Write),
    ("thread/goal/set", Write),
    ("thread/goal/get", Read),
    ("thread/goal/clear", Write),
    ("thread/metadata/update", Write),
    ("thread/attachment/add", Write),
    ("thread/attachment/list", Read),
    ("thread/attachmentOwner/list", Read),
    ("thread/attachment/remove", Write),
    ("thread/section/move", Write),
    ("thread/unarchive", Write),
    ("thread/compact/start", Write),
    ("thread/shellCommand", Hidden),
    ("thread/approveGuardianDeniedAction", Write),
    ("thread/revert", Write),
    ("thread/list", Read),
    ("threadSection/list", Read),
    ("threadSection/create", Write),
    ("threadSection/update", Write),
    ("threadSection/delete", Write),
    ("thread/loaded/list", Read),
    ("thread/read", Read),
    ("thread/turns/list", Read),
    ("thread/items/list", Read),
    ("thread/inject_items", Write),
    ("skills/list", Read),
    ("skills/extraRoots/set", Write),
    ("hooks/list", Read),
    ("marketplace/add", Write),
    ("marketplace/remove", Write),
    ("marketplace/upgrade", Write),
    ("plugin/list", Read),
    ("plugin/installed", Read),
    ("plugin/reconcile", Write),
    ("plugin/read", Read),
    ("plugin/skill/read", Read),
    ("plugin/share/save", Write),
    ("plugin/share/updateTargets", Write),
    ("plugin/share/list", Read),
    ("plugin/share/checkout", Write),
    ("plugin/share/delete", Write),
    ("app/read", Read),
    ("app/list", Read),
    ("app/installed", Read),
    ("fs/readFile", Hidden),
    ("fs/writeFile", Hidden),
    ("fs/createDirectory", Hidden),
    ("fs/getMetadata", Hidden),
    ("fs/readDirectory", Hidden),
    ("fs/remove", Hidden),
    ("fs/copy", Hidden),
    ("fs/watch", Hidden),
    ("fs/unwatch", Hidden),
    ("skills/config/write", Write),
    ("plugin/install", Write),
    ("plugin/uninstall", Write),
    ("turn/start", Write),
    ("turn/steer", Write),
    ("turn/interrupt", Write),
    ("review/start", Write),
    ("model/list", Read),
    ("account/gatewayOAuth/read", Read),
    ("account/gatewayOAuth/login", Hidden),
    ("account/gatewayOAuth/cancel", Hidden),
    ("modelProvider/capabilities/read", Read),
    ("experimentalFeature/list", Read),
    ("permissionProfile/list", Read),
    ("experimentalFeature/enablement/set", Write),
    ("mcpServer/oauth/login", Hidden),
    ("config/mcpServer/reload", Write),
    ("mcpServerStatus/list", Read),
    ("mcpServer/resource/read", Read),
    ("mcpServer/tool/call", Write),
    ("windowsSandbox/setupStart", Hidden),
    ("windowsSandbox/readiness", Read),
    ("account/login/start", Hidden),
    ("account/login/cancel", Hidden),
    ("account/logout", Write),
    ("account/rateLimits/read", Read),
    ("account/rateLimitResetCredit/consume", Write),
    ("account/usage/read", Read),
    ("account/workspaceMessages/read", Read),
    ("account/sendAddCreditsNudgeEmail", Write),
    ("feedback/upload", Hidden),
    ("command/exec", Hidden),
    ("command/exec/write", Hidden),
    ("command/exec/terminate", Hidden),
    ("command/exec/resize", Hidden),
    ("config/read", Read),
    ("externalAgentConfig/detect", Read),
    ("externalAgentConfig/import", Write),
    ("externalAgentConfig/import/recordHistory", Write),
    ("externalAgentConfig/import/readHistories", Read),
    ("config/value/write", Write),
    ("config/batchWrite", Write),
    ("configRequirements/read", Read),
    ("account/read", Read),
    ("fuzzyFileSearch", Read),
];

/// The client's only notification; sent once after `initialize`.
pub const CLIENT_NOTIFICATIONS: &[&str] = &["initialized"];

pub const SERVER_REQUESTS: &[(&str, Reply)] = &[
    ("item/commandExecution/requestApproval", Reply::Accept),
    ("item/fileChange/requestApproval", Reply::Accept),
    ("item/tool/requestUserInput", Reply::Cancel),
    ("mcpServer/elicitation/request", Reply::Cancel),
    ("item/permissions/requestApproval", Reply::Accept),
    ("item/tool/call", Reply::Fail),
    ("account/chatgptAuthTokens/refresh", Reply::Refuse),
    ("attestation/generate", Reply::Refuse),
    ("applyPatchApproval", Reply::Accept),
    ("execCommandApproval", Reply::Accept),
];

pub const SERVER_NOTIFICATIONS: &[(&str, Category)] = &[
    ("error", Turn),
    ("thread/started", Thread),
    ("thread/status/changed", Thread),
    ("thread/readState/changed", Thread),
    ("thread/archived", Thread),
    ("thread/deleted", Thread),
    ("thread/unarchived", Thread),
    ("thread/closed", Thread),
    ("thread/reverted", Thread),
    ("skills/changed", Skills),
    ("thread/name/updated", Thread),
    ("thread/attachment/updated", Thread),
    ("thread/goal/updated", Thread),
    ("thread/prediction/updated", Thread),
    ("thread/goal/cleared", Thread),
    ("thread/queue/changed", Thread),
    ("project/changed", Thread),
    ("thread/project/updated", Thread),
    ("thread/environment/connected", Thread),
    ("thread/environment/disconnected", Thread),
    ("thread/settings/updated", Thread),
    ("thread/tokenUsage/updated", Usage),
    ("turn/started", Turn),
    ("hook/started", Hook),
    ("turn/completed", Turn),
    ("hook/completed", Hook),
    ("turn/diff/updated", Output),
    ("turn/plan/updated", Plan),
    ("item/started", Item),
    ("item/autoApprovalReview/started", Review),
    ("item/autoApprovalReview/completed", Review),
    ("autoApprovalReview/strictReviewRequired", Review),
    ("item/completed", Item),
    ("item/agentMessage/delta", Item),
    ("item/plan/delta", Plan),
    ("command/exec/outputDelta", Output),
    ("process/outputDelta", Output),
    ("process/exited", Output),
    ("item/commandExecution/outputDelta", Output),
    ("item/commandExecution/terminalInteraction", Output),
    ("item/fileChange/outputDelta", Output),
    ("item/fileChange/patchUpdated", Output),
    ("serverRequest/resolved", Request),
    ("item/mcpToolCall/progress", Item),
    ("mcpServer/oauthLogin/completed", Mcp),
    ("mcpServer/startupStatus/updated", Mcp),
    ("mcpServer/event/stream/notification", Mcp),
    ("account/updated", Account),
    ("account/gatewayOAuth/changed", Account),
    ("account/rateLimits/updated", RateLimits),
    ("app/list/updated", Apps),
    ("remoteControl/status/changed", Platform),
    ("externalAgentConfig/import/progress", Import),
    ("externalAgentConfig/import/completed", Import),
    ("fs/changed", Output),
    ("item/reasoning/summaryTextDelta", Reasoning),
    ("item/reasoning/summaryPartAdded", Reasoning),
    ("item/reasoning/textDelta", Reasoning),
    ("thread/compacted", Thread),
    ("model/rerouted", Model),
    ("model/verification", Model),
    ("modelProvider/authRecoveryStarted", Account),
    ("modelProvider/authRecoveryCompleted", Account),
    ("turn/moderationMetadata", Turn),
    ("model/safetyBuffering/updated", Model),
    ("warning", Warning),
    ("guardianWarning", Warning),
    ("deprecationNotice", Warning),
    ("configWarning", Warning),
    ("fuzzyFileSearch/sessionUpdated", Search),
    ("fuzzyFileSearch/sessionCompleted", Search),
    ("thread/realtime/started", Realtime),
    ("thread/realtime/itemAdded", Realtime),
    ("thread/realtime/item/started", Realtime),
    ("thread/realtime/item/transcript/delta", Realtime),
    ("thread/realtime/item/completed", Realtime),
    ("thread/realtime/transcript/delta", Realtime),
    ("thread/realtime/transcript/done", Realtime),
    ("thread/realtime/outputAudio/delta", Realtime),
    ("thread/realtime/sdp", Realtime),
    ("thread/realtime/error", Realtime),
    ("thread/realtime/closed", Realtime),
    ("windows/worldWritableWarning", Platform),
    ("windowsSandbox/setupCompleted", Platform),
    ("account/login/completed", Account),
];

fn find<T: Copy>(table: &[(&str, T)], method: &str) -> Option<T> {
    table
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, value)| *value)
}

/// Unknown methods count as writes: a lost reply must never pass as harmless.
pub fn access(method: &str) -> Access {
    find(CLIENT_REQUESTS, method).unwrap_or(Write)
}

pub fn category(method: &str) -> Option<Category> {
    find(SERVER_NOTIFICATIONS, method)
}

/// The answer to a server request, as a JSON-RPC `result` or `error`. Unknown requests are
/// refused so they never hang.
pub fn reply(method: &str, params: &Value) -> (Result<Value, Value>, &'static str) {
    match find(SERVER_REQUESTS, method).unwrap_or(Reply::Refuse) {
        Reply::Accept => (
            Ok(match method {
                "item/permissions/requestApproval" => {
                    json!({"permissions":params["permissions"],"scope":"turn"})
                }
                "applyPatchApproval" | "execCommandApproval" => json!({"decision":"approved"}),
                _ => json!({"decision":"accept"}),
            }),
            "accept",
        ),
        Reply::Cancel => (
            Ok(match method {
                "mcpServer/elicitation/request" => json!({"action":"cancel"}),
                _ => json!({"answers":{}}),
            }),
            "cancel",
        ),
        Reply::Fail => (
            Ok(
                json!({"success":false,"contentItems":[{"type":"inputText","text":"this client provides no dynamic tools"}]}),
            ),
            "fail",
        ),
        Reply::Refuse => (
            Err(json!({"code":-32601,"message":"vibeface does not answer this request"})),
            "refuse",
        ),
    }
}

/// Removes what the browser must not see: executed commands and their output, diffs and changed
/// files. The observed state keeps them under `hidden`, which is dropped here.
pub fn sanitize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if matches!(
                map.get("type").and_then(Value::as_str),
                Some("commandExecution" | "fileChange" | "functionCallOutput")
            ) {
                map.retain(|key, _| matches!(key.as_str(), "id" | "type" | "status"));
                return;
            }
            map.remove("hidden");
            map.remove("diff");
            map.remove("aggregatedOutput");
            for item in map.values_mut() {
                sanitize(item);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(sanitize),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const SCHEMA: &str = include_str!("../schema/app-server-methods.json");
    const DOC: &str = include_str!("../../../docs/codex-app-server.md");

    fn names<T>(table: &[(&str, T)]) -> BTreeSet<String> {
        table.iter().map(|(name, _)| (*name).to_owned()).collect()
    }

    #[test]
    fn every_schema_method_is_classified_and_documented() {
        let schema: Value = serde_json::from_str(SCHEMA).unwrap();
        let listed = |key: &str| -> BTreeSet<String> {
            schema[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m.as_str().unwrap().to_owned())
                .collect()
        };
        let tables = [
            ("client_requests", names(CLIENT_REQUESTS)),
            (
                "client_notifications",
                CLIENT_NOTIFICATIONS.iter().map(|m| (*m).to_owned()).collect(),
            ),
            ("server_requests", names(SERVER_REQUESTS)),
            ("server_notifications", names(SERVER_NOTIFICATIONS)),
        ];
        for (key, table) in tables {
            let schema = listed(key);
            let missing: Vec<_> = schema.difference(&table).collect();
            let stale: Vec<_> = table.difference(&schema).collect();
            assert!(missing.is_empty(), "{key} not classified: {missing:?}");
            assert!(stale.is_empty(), "{key} no longer in the schema: {stale:?}");
            for method in &schema {
                assert!(
                    DOC.contains(&format!("| `{method}` |")),
                    "docs/codex-app-server.md does not list {method}"
                );
            }
        }
    }

    #[test]
    fn server_requests_get_deterministic_answers() {
        let (result, decision) = reply(
            "item/permissions/requestApproval",
            &json!({"permissions":{"network":{"enabled":true}}}),
        );
        assert_eq!(decision, "accept");
        assert_eq!(result.unwrap()["permissions"]["network"]["enabled"], true);
        assert_eq!(
            reply("mcpServer/elicitation/request", &json!({})).0.unwrap(),
            json!({"action":"cancel"})
        );
        assert_eq!(
            reply("item/tool/call", &json!({})).0.unwrap()["success"],
            false
        );
        assert!(reply("account/chatgptAuthTokens/refresh", &json!({})).0.is_err());
        assert!(reply("future/request", &json!({})).0.is_err());
        assert_eq!(access("future/method"), Write);
        assert_eq!(access("thread/read"), Read);
    }

    #[test]
    fn sanitize_drops_commands_output_and_diffs() {
        let mut value = json!({"items":[
            {"id":"a","type":"commandExecution","status":"completed","command":"rm -rf x","aggregatedOutput":"out"},
            {"id":"b","type":"fileChange","status":"completed","changes":[{"path":"a.rs"}]},
            {"id":"c","type":"agentMessage","text":"done"}],
            "hidden":{"diff":"+x"},"diff":"+y"});
        sanitize(&mut value);
        assert_eq!(
            value,
            json!({"items":[{"id":"a","type":"commandExecution","status":"completed"},
                {"id":"b","type":"fileChange","status":"completed"},
                {"id":"c","type":"agentMessage","text":"done"}]})
        );
    }
}
