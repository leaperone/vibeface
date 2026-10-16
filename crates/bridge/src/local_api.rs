use crate::runtime::Bridge;
use anyhow::{Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use futures_util::stream;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use vibeface_protocol::{
    BridgeEvent, ChatMessage, CommandRecord, CommandRequest, TaskRecord, validate_id,
};

const MAX_BODY_BYTES: usize = 128 * 1024;
const MAX_EVENTS: usize = 1_000;
const DEFAULT_EVENTS: usize = 100;

#[derive(Clone)]
struct App {
    bridge: Arc<Bridge>,
    token: Arc<str>,
}

#[derive(Debug)]
struct ApiError(StatusCode, String);
type ApiResult<T> = std::result::Result<T, ApiError>;

impl ApiError {
    fn bad(error: impl std::fmt::Display) -> Self {
        Self(StatusCode::BAD_REQUEST, error.to_string())
    }

    fn not_found(error: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, error.into())
    }

    fn conflict(error: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, error.into())
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "local bridge API operation failed");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "bridge operation failed".into(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

/// Serve the loopback/LAN HTTP API for a running Bridge.
///
/// The caller owns `listener`, so binding policy stays with the runtime. The
/// API never exposes the bearer token or cancels a command when its client
/// disconnects.
pub(crate) async fn serve(bridge: Arc<Bridge>, listener: TcpListener, token: String) -> Result<()> {
    ensure!(
        valid_token(&token),
        "local API token must be nonempty and contain no surrounding whitespace or newlines"
    );
    let app_state = App {
        bridge,
        token: Arc::from(token),
    };
    let protected = Router::new()
        .route("/v1/commands", post(submit))
        .route("/v1/commands/{id}", get(command))
        .route("/v1/status", get(status))
        .route("/v1/events", get(events))
        .route("/v1/stream", get(event_stream))
        .route("/v1/tasks", get(tasks))
        .route("/v1/chat", get(chat))
        .layer(middleware::from_fn_with_state(app_state.clone(), authorize));
    let app = Router::new()
        .route("/healthz", get(healthz))
        .merge(protected)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(app_state);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn authorize(State(app): State<App>, request: Request, next: Next) -> Response {
    if !authorized(request.headers(), &app.token) {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "valid bearer token required".into(),
        )
        .into_response();
    }
    next.run(request).await
}

async fn submit(
    State(app): State<App>,
    Json(request): Json<CommandRequest>,
) -> ApiResult<Response> {
    request.validate().map_err(ApiError::bad)?;
    let result = app
        .bridge
        .submit(request, false)
        .await
        .map_err(command_error)?;
    let status = if result.status == vibeface_protocol::CommandStatus::Unknown {
        StatusCode::CONFLICT
    } else {
        StatusCode::ACCEPTED
    };
    Ok((status, Json(result)).into_response())
}

async fn command(State(app): State<App>, Path(id): Path<String>) -> ApiResult<Json<CommandRecord>> {
    validate_id(&id).map_err(ApiError::bad)?;
    app.bridge
        .command(&id)
        .await
        .map_err(ApiError::internal)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("command not found"))
}

async fn status(State(app): State<App>) -> ApiResult<Json<Value>> {
    Ok(Json(
        app.bridge.snapshot().await.map_err(ApiError::internal)?,
    ))
}

#[derive(Default, Deserialize)]
struct CursorQuery {
    #[serde(default)]
    after: u64,
    limit: Option<usize>,
}

fn cursor(query: &CursorQuery) -> ApiResult<(u64, usize)> {
    let limit = query.limit.unwrap_or(DEFAULT_EVENTS);
    if query.after > i64::MAX as u64 || limit == 0 || limit > MAX_EVENTS {
        return Err(ApiError::bad(
            "after must fit a signed 64-bit integer and limit must be 1-1000",
        ));
    }
    Ok((query.after, limit))
}

async fn events(
    State(app): State<App>,
    Query(query): Query<CursorQuery>,
) -> ApiResult<Json<Vec<BridgeEvent>>> {
    let (after, limit) = cursor(&query)?;
    Ok(Json(
        app.bridge
            .events_after(after, limit)
            .await
            .map_err(ApiError::internal)?,
    ))
}

async fn tasks(State(app): State<App>) -> ApiResult<Json<Vec<TaskRecord>>> {
    Ok(Json(
        app.bridge.list_tasks().await.map_err(ApiError::internal)?,
    ))
}

async fn chat(
    State(app): State<App>,
    Query(query): Query<CursorQuery>,
) -> ApiResult<Json<Vec<ChatMessage>>> {
    let (after, limit) = cursor(&query)?;
    Ok(Json(
        app.bridge
            .chat_history(after, limit)
            .await
            .map_err(ApiError::internal)?,
    ))
}

async fn event_stream(
    State(app): State<App>,
    Query(query): Query<CursorQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (mut after, _) = cursor(&query)?;
    if let Some(value) = headers.get("last-event-id") {
        after = value
            .to_str()
            .map_err(ApiError::bad)?
            .parse::<u64>()
            .map_err(ApiError::bad)?;
        if after > i64::MAX as u64 {
            return Err(ApiError::bad(
                "last-event-id must fit a signed 64-bit integer",
            ));
        }
    }
    let state = StreamState {
        bridge: app.bridge,
        after,
    };
    let output = stream::unfold(state, |mut state| async move {
        loop {
            let next = match state.bridge.events_after(state.after, 1).await {
                Ok(mut events) => events.pop(),
                Err(error) => {
                    return Some((Err(io::Error::other(error.to_string())), state));
                }
            };
            if let Some(event) = next {
                state.after = event.seq;
                let data = match serde_json::to_string(&event) {
                    Ok(data) => data,
                    Err(error) => {
                        return Some((Err(io::Error::other(error.to_string())), state));
                    }
                };
                let output = Event::default()
                    .id(event.seq.to_string())
                    .event(event.kind)
                    .data(data);
                return Some((Ok(output), state));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
    Ok(Sse::new(output)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

struct StreamState {
    bridge: Arc<Bridge>,
    after: u64,
}

fn command_error(error: anyhow::Error) -> ApiError {
    let message = error.to_string();
    if message.contains("command id already exists with different content") {
        ApiError::conflict(message)
    } else if message.contains("internal scheduler command") {
        ApiError::bad(message)
    } else {
        ApiError::internal(message)
    }
}

fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.trim() == token && !token.contains(['\r', '\n'])
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return false;
    }
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };
    if value.len() != token.len() {
        return false;
    }
    value
        .bytes()
        .zip(token.bytes())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_auth_requires_exact_token() {
        let mut headers = HeaderMap::new();
        assert!(!authorized(&headers, "bridge-secret"));
        headers.insert(
            header::AUTHORIZATION,
            "Bearer bridge-secret".parse().unwrap(),
        );
        assert!(authorized(&headers, "bridge-secret"));
        assert!(!authorized(&headers, "other-secret"));
        assert!(!authorized(&headers, "bridge-secret "));
        headers.append(
            header::AUTHORIZATION,
            "Bearer bridge-secret".parse().unwrap(),
        );
        assert!(!authorized(&headers, "bridge-secret"));
        assert!(valid_token("bridge-secret"));
        assert!(!valid_token(" bridge-secret"));
        assert!(!valid_token("bridge\nsecret"));
    }

    #[test]
    fn cursor_rejects_unbounded_queries() {
        assert!(
            cursor(&CursorQuery {
                after: u64::MAX,
                limit: None
            })
            .is_err()
        );
        assert!(
            cursor(&CursorQuery {
                after: 0,
                limit: Some(0)
            })
            .is_err()
        );
        assert!(
            cursor(&CursorQuery {
                after: 0,
                limit: Some(MAX_EVENTS + 1)
            })
            .is_err()
        );
        assert_eq!(
            cursor(&CursorQuery::default()).unwrap(),
            (0, DEFAULT_EVENTS)
        );
    }
}
