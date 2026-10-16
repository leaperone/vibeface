use crate::runtime::{Access, Runtime};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State, WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
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
use std::{
    collections::HashMap,
    convert::Infallible,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, broadcast},
};

const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const COOKIE: &str = "vibeface_session";
const CALLBACK: &str = "/sdk.v1.SdkCustomToolCallbackService/CallCustomTool";
const AUTH_WINDOW: Duration = Duration::from_secs(60);
const AUTH_ATTEMPTS: u32 = 20;

#[derive(Clone)]
struct App {
    node: Arc<Runtime>,
    site: Option<Site>,
    attempts: Arc<Mutex<HashMap<IpAddr, (Instant, u32)>>>,
}

#[derive(Clone)]
struct Site {
    origin: String,
    authority: String,
    secure: bool,
}

impl Site {
    fn parse(public_url: &str) -> Result<Self> {
        let url = reqwest::Url::parse(public_url).context("invalid public_url")?;
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "public_url must use HTTP or HTTPS"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "public_url must not contain credentials"
        );
        ensure!(
            url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
            "public_url must be an origin without a path, query or fragment"
        );
        let origin = url.origin().ascii_serialization();
        let authority = origin
            .split_once("://")
            .context("public_url requires a host")?
            .1
            .to_owned();
        ensure!(!authority.is_empty(), "public_url requires a host");
        Ok(Self {
            origin,
            authority,
            secure: url.scheme() == "https",
        })
    }
}

#[derive(Debug)]
struct ApiError(StatusCode, String);
type ApiResult<T> = std::result::Result<T, ApiError>;

impl ApiError {
    fn auth() -> Self {
        Self(StatusCode::UNAUTHORIZED, "authentication required".into())
    }
    fn bad(error: impl std::fmt::Display) -> Self {
        Self(StatusCode::BAD_REQUEST, error.to_string())
    }
    fn forbidden() -> Self {
        Self(
            StatusCode::FORBIDDEN,
            "request origin or CSRF token is invalid".into(),
        )
    }
    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "node HTTP operation failed");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "node operation failed".into(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

pub async fn serve(node: Arc<Runtime>) -> Result<()> {
    let site = Site::parse(&node.config.public_url)?;
    ensure!(
        site.secure || node.config.listen.ip().is_loopback() || node.config.allow_insecure_lan,
        "non-loopback HTTP requires allow_insecure_lan; use HTTPS for public access"
    );
    let listener = TcpListener::bind(node.config.listen).await?;
    axum::serve(
        listener,
        router(node).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

pub fn router(node: Arc<Runtime>) -> Router {
    let app = App {
        site: Site::parse(&node.config.public_url).ok(),
        node,
        attempts: Arc::new(Mutex::new(HashMap::new())),
    };
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(javascript))
        .route("/style.css", get(stylesheet))
        .route("/api/info", get(info))
        .route("/api/register", post(register))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/pair", post(pair))
        .route("/api/rpc", post(rpc))
        .route("/api/tools", post(tool))
        .route(
            CALLBACK,
            post(tool_callback).layer(DefaultBodyLimit::max(MAX_BODY_BYTES / 2)),
        )
        .route("/api/events", get(events))
        .route("/peer", get(peer))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(app.clone(), boundary))
        .with_state(app)
}

async fn boundary(State(app): State<App>, request: Request, next: Next) -> Response {
    let ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|peer| peer.0.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let mut response = match check_boundary(
        &app,
        request.headers(),
        request.method(),
        request.uri().path(),
        ip,
    )
    .await
    {
        Ok(()) => next.run(request).await,
        Err(error) => error.into_response(),
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
    response
}

async fn check_boundary(
    app: &App,
    headers: &HeaderMap,
    method: &Method,
    path: &str,
    ip: IpAddr,
) -> ApiResult<()> {
    let site = app
        .site
        .as_ref()
        .ok_or_else(|| ApiError::internal("invalid configured public_url"))?;
    check_site(site, headers)?;
    let unsafe_method = !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    if unsafe_method {
        let content_type = one_header(headers, header::CONTENT_TYPE.as_str())
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        if content_type != Some("application/json") {
            return Err(ApiError(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "application/json required".into(),
            ));
        }
        let is_tool_or_rpc = matches!(path, "/api/rpc" | "/api/tools") || path == CALLBACK;
        let bearer_call = is_tool_or_rpc && bearer(headers).is_some();
        if !bearer_call
            && one_header(headers, header::ORIGIN.as_str()) != Some(site.origin.as_str())
        {
            return Err(ApiError::forbidden());
        }
    }
    if matches!(path, "/api/register" | "/api/login" | "/api/pair") && *method == Method::POST {
        let now = Instant::now();
        let mut attempts = app.attempts.lock().await;
        attempts.retain(|_, (started, _)| now.duration_since(*started) < AUTH_WINDOW);
        if attempts.len() >= 4096 && !attempts.contains_key(&ip) {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "too many authentication attempts".into(),
            ));
        }
        let (_, count) = attempts.entry(ip).or_insert((now, 0));
        if *count >= AUTH_ATTEMPTS {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "too many authentication attempts".into(),
            ));
        }
        *count += 1;
    }
    Ok(())
}

fn check_site(site: &Site, headers: &HeaderMap) -> ApiResult<()> {
    let host = one_header(headers, header::HOST.as_str()).ok_or_else(ApiError::forbidden)?;
    if !host.eq_ignore_ascii_case(&site.authority) {
        return Err(ApiError::forbidden());
    }
    if headers.contains_key(header::ORIGIN)
        && one_header(headers, header::ORIGIN.as_str()) != Some(site.origin.as_str())
    {
        return Err(ApiError::forbidden());
    }
    Ok(())
}

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let token = one_header(headers, header::AUTHORIZATION.as_str())?.strip_prefix("Bearer ")?;
    (!token.is_empty() && token.trim() == token).then_some(token)
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        for cookie in value.to_str().ok()?.split(';') {
            let Some((name, value)) = cookie.trim().split_once('=') else {
                continue;
            };
            if name == COOKIE {
                if found.is_some() || value.is_empty() {
                    return None;
                }
                found = Some(value.to_owned());
            }
        }
    }
    found
}

fn same_secret(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |different, (left, right)| different | (left ^ right))
            == 0
}

async fn access(app: &App, headers: &HeaderMap, mutation: bool) -> ApiResult<Access> {
    if headers.contains_key(header::AUTHORIZATION) {
        return bearer_access(app, headers).await;
    }
    let token = cookie_token(headers).ok_or_else(ApiError::auth)?;
    let session = app
        .node
        .auth
        .lock()
        .await
        .authenticate(&token)
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::auth)?;
    if mutation
        && !one_header(headers, "x-csrf-token").is_some_and(|csrf| same_secret(csrf, &session.csrf))
    {
        return Err(ApiError::forbidden());
    }
    Ok(Access {
        network_id: session.account.network_id,
        human: true,
        conversation_id: None,
    })
}

async fn bearer_access(app: &App, headers: &HeaderMap) -> ApiResult<Access> {
    let token = bearer(headers).ok_or_else(ApiError::auth)?;
    app.node
        .authorize_bearer(token)
        .await
        .ok_or_else(ApiError::auth)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    username: String,
    password: String,
}

async fn info(State(app): State<App>) -> Json<Value> {
    Json(app.node.info())
}

async fn register(State(app): State<App>, Json(input): Json<Credentials>) -> ApiResult<Response> {
    let account = app
        .node
        .register(&input.username, &input.password)
        .await
        .map_err(ApiError::bad)?;
    Ok((StatusCode::CREATED, Json(json!({"account": account}))).into_response())
}

async fn login(State(app): State<App>, Json(input): Json<Credentials>) -> ApiResult<Response> {
    let login = app
        .node
        .auth
        .lock()
        .await
        .login(&input.username, &input.password)
        .map_err(|_| ApiError::auth())?;
    let mut response = Json(json!({"account": login.account, "csrf": login.csrf})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(
            &login.token,
            app.site.as_ref().is_some_and(|site| site.secure),
            false,
        )?,
    );
    Ok(response)
}

fn session_cookie(token: &str, secure: bool, remove: bool) -> ApiResult<HeaderValue> {
    if !remove
        && (token.is_empty()
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    {
        return Err(ApiError::internal("invalid session token encoding"));
    }
    let secure_flag = if secure { "; Secure" } else { "" };
    let expires = if remove { "; Max-Age=0" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict{secure_flag}{expires}"
    ))
    .map_err(ApiError::internal)
}

async fn logout(State(app): State<App>, headers: HeaderMap) -> ApiResult<Response> {
    if headers.contains_key(header::AUTHORIZATION) {
        return Err(ApiError::auth());
    }
    access(&app, &headers, true).await?;
    let token = cookie_token(&headers).ok_or_else(ApiError::auth)?;
    app.node
        .auth
        .lock()
        .await
        .logout(&token)
        .map_err(ApiError::internal)?;
    let mut response = Json(json!({"ok": true})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie("", app.site.as_ref().is_some_and(|site| site.secure), true)?,
    );
    Ok(response)
}

async fn me(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let token = cookie_token(&headers).ok_or_else(ApiError::auth)?;
    let session = app
        .node
        .auth
        .lock()
        .await
        .authenticate(&token)
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::auth)?;
    Ok(Json(
        json!({"account": session.account, "csrf": session.csrf}),
    ))
}

async fn pair(State(app): State<App>, Json(input): Json<Value>) -> ApiResult<Json<Value>> {
    Ok(Json(app.node.pair(input).await.map_err(ApiError::bad)?))
}

async fn rpc(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> ApiResult<Json<Value>> {
    let access = access(&app, &headers, true).await?;
    Ok(Json(
        app.node.rpc(access, input).await.map_err(ApiError::bad)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolRequest {
    name: String,
    arguments: Value,
    call_id: String,
}

async fn tool(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<ToolRequest>,
) -> ApiResult<Json<Value>> {
    let access = bearer_access(&app, &headers).await?;
    validate_tool_call(&input.name, &input.call_id)?;
    Ok(Json(
        app.node
            .tool(access, &input.name, input.arguments, &input.call_id)
            .await
            .map_err(ApiError::bad)?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallback {
    tool_name: String,
    args: Value,
    tool_call_id: Option<String>,
    agent_id: String,
}

async fn tool_callback(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<ToolCallback>,
) -> ApiResult<Json<Value>> {
    let access = bearer_access(&app, &headers).await?;
    if !input.args.is_object() || input.agent_id.is_empty() || input.agent_id.len() > 256 {
        return Err(ApiError::bad(
            "args must be an object and agentId must be nonempty and bounded",
        ));
    }
    // Executor writes still require the operation ID in args; this ID only identifies the callback.
    let call_id = input
        .tool_call_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_tool_call(&input.tool_name, &call_id)?;
    let result = app
        .node
        .tool(access, &input.tool_name, input.args, &call_id)
        .await
        .map_err(ApiError::bad)?;
    Ok(Json(json!({"result": result})))
}

fn validate_tool_call(name: &str, call_id: &str) -> ApiResult<()> {
    if name.is_empty() || name.len() > 128 || call_id.is_empty() || call_id.len() > 256 {
        return Err(ApiError::bad(
            "tool name and call ID must be nonempty and bounded",
        ));
    }
    Ok(())
}

async fn events(State(app): State<App>, headers: HeaderMap) -> ApiResult<Response> {
    let authorization = access(&app, &headers, false).await?;
    let receiver = app.node.events.subscribe();
    let output = stream::unfold(
        (app, headers, authorization.network_id, receiver),
        |(app, headers, network_id, mut receiver)| async move {
            loop {
                let update = tokio::time::timeout(Duration::from_secs(30), receiver.recv()).await;
                if !access(&app, &headers, false)
                    .await
                    .is_ok_and(|current| current.network_id == network_id)
                {
                    return None;
                }
                let event = match update {
                    Err(_) => continue,
                    Ok(Err(broadcast::error::RecvError::Closed)) => return None,
                    Ok(Err(broadcast::error::RecvError::Lagged(_))) => Event::default()
                        .event("reset")
                        .data("{\"reason\":\"sync_required\"}"),
                    Ok(Ok(update)) if update.network_id == network_id => Event::default()
                        .event("update")
                        .data(update.payload.to_string()),
                    Ok(Ok(_)) => continue,
                };
                return Some((
                    Ok::<_, Infallible>(event),
                    (app, headers, network_id, receiver),
                ));
            }
        },
    );
    Ok(Sse::new(output)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

async fn peer(State(app): State<App>, websocket: WebSocketUpgrade) -> Response {
    websocket
        .max_message_size(MAX_BODY_BYTES)
        .max_frame_size(MAX_BODY_BYTES)
        .on_upgrade(move |socket| async move {
            if let Err(error) = app.node.hub.clone().accept(socket).await {
                tracing::warn!(%error, "peer connection closed");
            }
        })
}

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../web/index.html"),
    )
}

async fn javascript() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/app.js"),
    )
}

async fn stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../web/style.css"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_and_host_block_cross_site_and_rebinding() {
        let site = Site::parse("http://127.0.0.1:8787").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8787".parse().unwrap());
        assert!(check_site(&site, &headers).is_ok());
        headers.insert(header::ORIGIN, "https://hostile.invalid".parse().unwrap());
        assert!(check_site(&site, &headers).is_err());
        headers.insert(header::ORIGIN, "http://127.0.0.1:8787".parse().unwrap());
        headers.insert(header::HOST, "hostile.invalid:8787".parse().unwrap());
        assert!(check_site(&site, &headers).is_err());
        headers.insert(header::HOST, "127.0.0.1:8787".parse().unwrap());
        headers.append(header::HOST, "hostile.invalid".parse().unwrap());
        assert!(check_site(&site, &headers).is_err());
        assert!(Site::parse("https://example.com/path").is_err());
        assert!(Site::parse("https://user:secret@example.com").is_err());
    }

    #[test]
    fn session_cookies_are_opaque_and_reject_ambiguous_tokens() {
        let cookie = session_cookie("secret", true, false).unwrap();
        assert_eq!(
            cookie.to_str().unwrap(),
            "vibeface_session=secret; Path=/; HttpOnly; SameSite=Strict; Secure"
        );
        assert!(session_cookie("secret; Domain=example.com", false, false).is_err());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "other=1; vibeface_session=secret".parse().unwrap(),
        );
        assert_eq!(cookie_token(&headers).as_deref(), Some("secret"));
        headers.append(header::COOKIE, "vibeface_session=other".parse().unwrap());
        assert_eq!(cookie_token(&headers), None);
        assert!(same_secret("token", "token"));
        assert!(!same_secret("token", "other"));
        assert!(!same_secret("token", "token-long"));
    }

    #[test]
    fn authorization_requires_one_exact_bearer_header() {
        let mut headers = HeaderMap::new();
        assert!(bearer(&headers).is_none());
        headers.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert_eq!(bearer(&headers), Some("secret"));
        headers.append(header::AUTHORIZATION, "Bearer second".parse().unwrap());
        assert!(bearer(&headers).is_none());
        assert!(validate_tool_call("dispatch", "stable-call-id").is_ok());
        assert!(validate_tool_call("dispatch", "").is_err());
    }
}
