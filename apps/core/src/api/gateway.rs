//! Caller-keyed REST and stateless Streamable HTTP MCP for the local executor facade.
use super::*;
use crate::runtime::executor_gateway::{Caller, GatewayError};
use axum::extract::{Path as RoutePath, rejection::JsonRejection};

const PREFIX: &str = "/api/gateway/v1";
const OPERATIONS: [&str; 8] = [
    "capabilities",
    "start",
    "status",
    "wait",
    "steer",
    "queue",
    "stop",
    "result",
];

pub(super) fn is_path(path: &str) -> bool {
    path.starts_with("/api/gateway/")
}

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route(&format!("{PREFIX}/callers"), post(mint))
        .route(&format!("{PREFIX}/capabilities"), get(capabilities))
        .route(&format!("{PREFIX}/{{operation}}"), post(call))
        .route("/api/gateway/mcp", post(mcp))
}

pub(super) fn failure(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(error_body(code, message))).into_response()
}

fn error_body(code: &str, message: &str) -> Value {
    json!({"code":code,"message":crate::logging::scrub(message)})
}

fn from_error(error: anyhow::Error) -> Response {
    let (code, message) = gateway_error(&error);
    failure(error_status(code), code, &message)
}

fn gateway_error(error: &anyhow::Error) -> (&'static str, String) {
    match error.downcast_ref::<GatewayError>() {
        Some(error) => (error.code, error.message.clone()),
        None => ("internal_error", "gateway operation failed".into()),
    }
}

fn error_status(code: &str) -> StatusCode {
    match code {
        "unauthorized" => StatusCode::UNAUTHORIZED,
        "owner_required" | "cwd_not_allowed" | "write_forbidden" => StatusCode::FORBIDDEN,
        "work_not_found" => StatusCode::NOT_FOUND,
        "not_running" | "idempotency_conflict" | "thread_busy" | "submission_unverified" => {
            StatusCode::CONFLICT
        }
        "timeout" => StatusCode::REQUEST_TIMEOUT,
        "internal_error" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    }
}

fn authenticate(app: &App, headers: &HeaderMap) -> std::result::Result<Caller, Response> {
    let key = bearer(headers).ok_or_else(|| {
        failure(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "caller key required",
        )
    })?;
    app.node.gateway.authenticate(key).map_err(from_error)
}

fn body(
    input: std::result::Result<Json<Value>, JsonRejection>,
) -> std::result::Result<Value, Response> {
    input.map(|Json(value)| value).map_err(|error| {
        let status = error.status();
        failure(status, "invalid_request", "request body must be valid JSON")
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mint {
    #[serde(default)]
    can_write: bool,
    webhook_url: Option<String>,
    webhook_secret: Option<String>,
}

async fn mint(
    State(app): State<App>,
    headers: HeaderMap,
    input: std::result::Result<Json<Value>, JsonRejection>,
) -> Response {
    if !app.local || !bearer(&headers).is_some_and(|key| app.node.local_access.is_owner(key)) {
        return failure(
            StatusCode::FORBIDDEN,
            "owner_required",
            "local owner token required",
        );
    }
    let value = match body(input) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let input: Mint = match serde_json::from_value(value) {
        Ok(input) => input,
        Err(_) => {
            return failure(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid caller options",
            );
        }
    };
    match app.node.gateway.mint(
        input.can_write,
        input.webhook_url.as_deref(),
        input.webhook_secret.as_deref(),
    ) {
        Ok(caller) => (StatusCode::CREATED, Json(caller)).into_response(),
        Err(error) => from_error(error),
    }
}

async fn capabilities(State(app): State<App>, headers: HeaderMap) -> Response {
    execute(&app, &headers, "capabilities", Ok(Json(json!({})))).await
}

async fn call(
    State(app): State<App>,
    RoutePath(operation): RoutePath<String>,
    headers: HeaderMap,
    input: std::result::Result<Json<Value>, JsonRejection>,
) -> Response {
    execute(&app, &headers, &operation, input).await
}

async fn execute(
    app: &App,
    headers: &HeaderMap,
    operation: &str,
    input: std::result::Result<Json<Value>, JsonRejection>,
) -> Response {
    let caller = match authenticate(app, headers) {
        Ok(caller) => caller,
        Err(error) => return error,
    };
    if !OPERATIONS.contains(&operation) {
        return failure(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "unknown gateway operation",
        );
    }
    let value = match body(input) {
        Ok(value) => value,
        Err(error) => return error,
    };
    match app.node.gateway_call(&caller, operation, value).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => from_error(error),
    }
}

fn tools() -> Vec<Value> {
    OPERATIONS.into_iter().map(|name| {
        let mut properties = json!({});
        let required: &[&str] = match name {
            "start" => {
                properties = json!({
                    "call_ref":{"type":"string"},"provider":{"enum":["codex","claude"]},
                    "cwd":{"type":"string"},"prompt":{"type":"string"},"title":{"type":"string"},
                    "model":{"type":"string"},"effort":{"type":"string"},"write":{"type":"boolean","default":false},
                    "thread":{"type":"string"},"continues":{"type":"string"},
                    "node":{"type":"string"},"target":{"type":"string"},"target_node":{"type":"string"}
                });
                &["call_ref","provider","cwd","prompt"]
            },
            "capabilities" => &[],
            "wait" => &["work_id","timeout_ms"],
            "queue" => &["work_id","call_ref","prompt","timeout_ms"],
            "steer" => &["work_id","call_ref","prompt"],
            _ => &["work_id"],
        };
        for key in required {
            if properties.get(*key).is_none() {
                properties[*key] = if *key == "timeout_ms" {
                    json!({"type":"integer","minimum":1,"maximum":120000})
                } else {
                    json!({"type":"string"})
                };
            }
        }
        json!({"name":name,"description":format!("Local executor gateway {name}. Call capabilities for provider operations, sandbox modes, overrides and limits. Write requires both caller permission and explicit write:true."),
            "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
    }).collect()
}

async fn mcp(
    State(app): State<App>,
    headers: HeaderMap,
    input: std::result::Result<Json<Value>, JsonRejection>,
) -> Response {
    let caller = match authenticate(&app, &headers) {
        Ok(caller) => caller,
        Err(error) => return error,
    };
    let input = match body(input) {
        Ok(input) => input,
        Err(error) => return error,
    };
    let id = input.get("id").cloned().unwrap_or(Value::Null);
    let error = |code, message: &str| {
        Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
            .into_response()
    };
    if input["jsonrpc"] != "2.0" || !matches!(id, Value::Null | Value::String(_) | Value::Number(_))
    {
        return error(-32600, "Invalid Request");
    }
    let Some(method) = input["method"].as_str() else {
        return error(-32600, "Invalid Request");
    };
    if id.is_null() {
        return if method.starts_with("notifications/") {
            StatusCode::ACCEPTED.into_response()
        } else {
            error(-32600, "A request ID is required")
        };
    }
    let result = match method {
        "initialize" => {
            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":false}},
            "serverInfo":{"name":"vibeface-executor-gateway","version":env!("CARGO_PKG_VERSION")}})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools":tools()}),
        "tools/call" => {
            let Some(name) = input["params"]["name"].as_str() else {
                return error(-32602, "A tool name is required");
            };
            if !OPERATIONS.contains(&name) {
                return error(-32602, "Unknown gateway tool");
            }
            let args = input["params"]
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !args.is_object() {
                return error(-32602, "Tool arguments must be an object");
            }
            let (value, failed) = match app.node.gateway_call(&caller, name, args).await {
                Ok(value) => (value, false),
                Err(error) => {
                    let (code, message) = gateway_error(&error);
                    (error_body(code, &message), true)
                }
            };
            json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":failed})
        }
        _ => return error(-32601, "Method not found"),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}

#[cfg(test)]
mod tests;
