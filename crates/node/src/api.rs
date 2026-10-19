use crate::{
    identity::local::BootstrapCapability,
    pairing::{Invitation, JoinState, NodePairing},
    runtime::{Access, Runtime},
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State, WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::net::TcpListener;

const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const CALLBACK: &str = "/sdk.v1.SdkCustomToolCallbackService/CallCustomTool";

#[derive(Clone)]
struct App {
    node: Arc<Runtime>,
    sites: Vec<Site>,
    pairing: Arc<NodePairing>,
}

#[derive(Clone)]
struct Site {
    origin: String,
    authority: String,
    secure: bool,
    loopback: bool,
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
            loopback: url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }),
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
        Self(StatusCode::FORBIDDEN, "request origin is invalid".into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

pub async fn serve(node: Arc<Runtime>) -> Result<()> {
    let site = Site::parse(&node.config.public_url)?;
    let sites = node
        .config
        .browser_origins()?
        .into_iter()
        .map(|origin| Site::parse(&origin))
        .collect::<Result<Vec<_>>>()?;
    ensure!(!sites.is_empty(), "at least one browser origin is required");
    ensure!(
        site.secure || node.config.listen.ip().is_loopback() || node.config.allow_insecure_lan,
        "non-loopback HTTP requires allow_insecure_lan; use HTTPS for public access"
    );
    let listener = TcpListener::bind(node.config.listen).await?;
    let local_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let local_url = format!("http://{}", local_listener.local_addr()?);
    node.local_access
        .publish_listener(local_url.clone(), node.identity.peer_id()?)?;
    let pairing = Arc::new(NodePairing::new(node.clone()));
    let local = browser_router(
        node.clone(),
        vec![Site::parse(&local_url)?],
        true,
        pairing.clone(),
    );
    let tools = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let tool_url = format!("http://{}", tools.local_addr()?);
    let internal = tool_router(node.clone(), Site::parse(&tool_url)?);
    node.tool_url
        .set(tool_url)
        .map_err(|_| anyhow::anyhow!("tool listener is already initialized"))?;
    tokio::try_join!(
        async {
            axum::serve(
                listener,
                browser_router(node.clone(), sites, false, pairing)
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        },
        async {
            axum::serve(
                local_listener,
                local.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        },
        async {
            axum::serve(
                tools,
                internal.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        }
    )?;
    Ok(())
}

fn tool_router(node: Arc<Runtime>, site: Site) -> Router {
    let app = App {
        pairing: Arc::new(NodePairing::new(node.clone())),
        node,
        sites: vec![site],
    };
    Router::new()
        .route("/api/tools", post(tool))
        .route(
            CALLBACK,
            post(tool_callback).layer(DefaultBodyLimit::max(MAX_BODY_BYTES / 2)),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(app.clone(), boundary))
        .with_state(app)
}

pub fn router(node: Arc<Runtime>) -> Router {
    let sites = node
        .config
        .browser_origins()
        .ok()
        .and_then(|origins| {
            origins
                .into_iter()
                .map(|origin| Site::parse(&origin))
                .collect::<Result<Vec<_>>>()
                .ok()
        })
        .unwrap_or_default();
    router_with_sites(node, sites)
}

fn router_with_sites(node: Arc<Runtime>, sites: Vec<Site>) -> Router {
    let pairing = Arc::new(NodePairing::new(node.clone()));
    browser_router(node, sites, false, pairing)
}

fn browser_router(
    node: Arc<Runtime>,
    sites: Vec<Site>,
    local: bool,
    pairing: Arc<NodePairing>,
) -> Router {
    let app = App {
        sites,
        node,
        pairing,
    };
    let mut routes = Router::new()
        .route("/transport.js", get(transport_javascript))
        .route("/generated/vibeface_secure.js", get(crypto_javascript))
        .route("/generated/vibeface_secure_bg.wasm", get(crypto_wasm))
        .route("/api/info", get(info))
        .route("/api/local/setup", post(local_setup))
        .route("/api/local/join/request", post(local_join_request))
        .route("/api/local/join/accept", post(local_join_accept))
        .route("/api/local/join/status", post(local_join_status))
        .route("/peer", get(peer))
        .route("/api/network/ws", get(network))
        .fallback(frontend);
    if local {
        routes = routes
            .route("/api/local/info", get(local_info))
            .route("/api/local/launch", post(local_launch))
            .route("/api/local/exchange", post(local_exchange))
            .route("/api/local/ws", get(local_websocket));
    }
    routes
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
        Ok(()) => {
            let work = if matches!(
                *request.method(),
                Method::GET | Method::HEAD | Method::OPTIONS
            ) {
                Ok(None)
            } else {
                app.node.maintenance.enter().map(Some)
            };
            match work {
                Ok(_work) => next.run(request).await,
                Err(_) => ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "node is preparing an update; try again after it restarts".into(),
                )
                .into_response(),
            }
        }
        Err(error) => error.into_response(),
    };
    let headers = response.headers_mut();
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(web_assets::CONTENT_SECURITY_POLICY),
    );
    response
}

async fn check_boundary(
    app: &App,
    headers: &HeaderMap,
    method: &Method,
    path: &str,
    ip: IpAddr,
) -> ApiResult<()> {
    let site = if path == "/api/network/ws" && *method == Method::GET {
        let host = one_header(headers, header::HOST.as_str()).ok_or_else(ApiError::forbidden)?;
        app.sites
            .iter()
            .find(|site| host.eq_ignore_ascii_case(&site.authority))
            .ok_or_else(ApiError::forbidden)?
    } else {
        check_site(&app.sites, headers)?
    };
    if !http_site_allowed(site, app.node.config.allow_insecure_lan, ip) {
        return Err(ApiError::forbidden());
    }
    if path == "/api/local/setup" || path.starts_with("/api/local/join/") {
        check_local_setup(site, headers, ip)?;
    }
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
        let is_tool = path == "/api/tools" || path == CALLBACK || path == "/api/local/launch";
        let bearer_call = is_tool && bearer(headers).is_some();
        if !bearer_call
            && !one_header(headers, header::ORIGIN.as_str())
                .is_some_and(|origin| site.origin.eq_ignore_ascii_case(origin))
        {
            return Err(ApiError::forbidden());
        }
    }
    Ok(())
}

fn check_site<'a>(sites: &'a [Site], headers: &HeaderMap) -> ApiResult<&'a Site> {
    let host = one_header(headers, header::HOST.as_str()).ok_or_else(ApiError::forbidden)?;
    let site = sites
        .iter()
        .find(|site| host.eq_ignore_ascii_case(&site.authority))
        .ok_or_else(ApiError::forbidden)?;
    if headers.contains_key(header::ORIGIN)
        && !one_header(headers, header::ORIGIN.as_str())
            .is_some_and(|origin| site.origin.eq_ignore_ascii_case(origin))
    {
        return Err(ApiError::forbidden());
    }
    Ok(site)
}

fn http_site_allowed(site: &Site, allow_insecure_lan: bool, peer: IpAddr) -> bool {
    site.secure || allow_insecure_lan || peer.is_loopback()
}

fn check_local_setup(site: &Site, headers: &HeaderMap, peer: IpAddr) -> ApiResult<()> {
    if !peer.is_loopback()
        || !site.loopback
        || one_header(headers, header::ORIGIN.as_str()) != Some(site.origin.as_str())
        || headers.keys().any(|name| {
            name == "forwarded" || name == "x-real-ip" || name.as_str().starts_with("x-forwarded-")
        })
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

async fn bearer_access(app: &App, headers: &HeaderMap) -> ApiResult<Access> {
    let token = bearer(headers).ok_or_else(ApiError::auth)?;
    app.node
        .authorize_bearer(token)
        .await
        .ok_or_else(ApiError::auth)
}

async fn info(State(app): State<App>) -> Json<Value> {
    Json(app.node.info())
}

async fn local_info(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let peer_id = app.node.identity.peer_id().map_err(ApiError::bad)?;
    let network_id = if app.node.has_membership().await.map_err(ApiError::bad)? {
        Some(
            app.node
                .membership_state()
                .await
                .map_err(ApiError::bad)?
                .genesis
                .network_id,
        )
    } else {
        None
    };
    let local_session = network_id.as_ref().and_then(|network_id| {
        local_cookie(&headers, &peer_id).and_then(|cookie| {
            app.node
                .local_access
                .session(cookie, &peer_id, network_id)
                .ok()
        })
    });
    Ok(Json(
        json!({"enabled":app.node.local_access.enabled(),"has_session":local_session.is_some(),"subject_id":local_session.map(|session| session.subject_id),"network_id":network_id,"entry":{"address":app.node.local_access.origin().map_err(ApiError::bad)?,"peer_id":peer_id,"relay":false,"local":true}}),
    ))
}

async fn local_launch(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let capability = bearer(&headers).ok_or_else(ApiError::auth)?;
    let state = app.node.membership_state().await.map_err(ApiError::bad)?;
    let peer_id = app.node.identity.peer_id().map_err(ApiError::bad)?;
    let token = app
        .node
        .local_access
        .issue(capability, &peer_id, &state.genesis.network_id)
        .map_err(|_| ApiError::auth())?;
    Ok(Json(
        json!({"url":app.node.local_access.origin().map_err(ApiError::bad)?,"peer_id":peer_id,"token":token}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalExchange {
    token: String,
    peer_id: String,
    subject_id: String,
    public_key: String,
}

async fn local_exchange(
    State(app): State<App>,
    Json(input): Json<LocalExchange>,
) -> ApiResult<Response> {
    let peer_id = app.node.identity.peer_id().map_err(ApiError::bad)?;
    if input.peer_id != peer_id {
        return Err(ApiError::auth());
    }
    let state = app.node.membership_state().await.map_err(ApiError::bad)?;
    let cookie = app
        .node
        .local_access
        .exchange(
            &input.token,
            &peer_id,
            &state.genesis.network_id,
            &input.subject_id,
            &input.public_key,
        )
        .map_err(|_| ApiError::auth())?;
    let mut response = Json(json!({"entry":{"address":app.node.local_access.origin().map_err(ApiError::bad)?,"peer_id":peer_id,"relay":false,"local":true},"network_id":state.genesis.network_id})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{}={cookie}; HttpOnly; SameSite=Strict; Path=/api/local; Max-Age={}",
            crate::local_access::cookie_name(&peer_id),
            crate::local_access::SESSION_SECONDS
        ))
        .map_err(ApiError::bad)?,
    );
    Ok(response)
}

fn local_cookie<'a>(headers: &'a HeaderMap, peer_id: &str) -> Option<&'a str> {
    let name = crate::local_access::cookie_name(peer_id);
    let mut cookies = one_header(headers, header::COOKIE.as_str())?
        .split(';')
        .filter_map(|cookie| cookie.trim().split_once('='))
        .filter(|(cookie_name, _)| *cookie_name == name);
    let (_, value) = cookies.next()?;
    cookies.next().is_none().then_some(value)
}

async fn local_websocket(
    State(app): State<App>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> ApiResult<Response> {
    let peer_id = app.node.identity.peer_id().map_err(ApiError::bad)?;
    let cookie = local_cookie(&headers, &peer_id)
        .ok_or_else(ApiError::auth)?
        .to_owned();
    let state = app.node.membership_state().await.map_err(ApiError::bad)?;
    app.node
        .local_access
        .session(&cookie, &peer_id, &state.genesis.network_id)
        .map_err(|_| ApiError::auth())?;
    Ok(websocket
        .max_message_size(65535)
        .max_frame_size(65535)
        .on_upgrade(move |socket| async move {
            if let Err(error) = crate::client::serve_local(
                app.node,
                crate::network::NetworkSocket::server(socket),
                cookie,
            )
            .await
            {
                tracing::debug!(%error, "local browser connection closed");
            }
        }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalSetupRequest {}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LocalSetup {
    Uninitialized {
        invitation: BootstrapInvitation,
    },
    Initialized {
        entry: LocalEntry,
        #[serde(skip_serializing_if = "Option::is_none")]
        local_launch: Option<LocalLaunch>,
    },
}

#[derive(Serialize)]
struct LocalLaunch {
    url: String,
    peer_id: String,
    token: String,
}

#[derive(Serialize)]
struct LocalEntry {
    address: String,
    peer_id: String,
    relay: bool,
}

#[derive(Serialize)]
struct BootstrapInvitation {
    url: String,
    peer_id: String,
    capability: String,
}

async fn local_setup(
    State(app): State<App>,
    headers: HeaderMap,
    Json(_): Json<LocalSetupRequest>,
) -> ApiResult<Json<LocalSetup>> {
    let origin = one_header(&headers, header::ORIGIN.as_str()).ok_or_else(ApiError::forbidden)?;
    let peer_id = app.node.identity.peer_id().map_err(ApiError::bad)?;
    if app.node.has_membership().await.map_err(ApiError::bad)? {
        let local_launch = if app.node.local_access.enabled() {
            match app.node.local_access.origin() {
                Ok(url) => {
                    let network_id = app
                        .node
                        .membership_state()
                        .await
                        .map_err(ApiError::bad)?
                        .genesis
                        .network_id;
                    let token = app
                        .node
                        .local_access
                        .issue_from_local_setup(&peer_id, &network_id)
                        .map_err(ApiError::bad)?;
                    Some(LocalLaunch {
                        url,
                        peer_id: peer_id.clone(),
                        token,
                    })
                }
                Err(_) => None,
            }
        } else {
            None
        };
        return Ok(Json(LocalSetup::Initialized {
            entry: LocalEntry {
                address: origin.into(),
                peer_id,
                relay: false,
            },
            local_launch,
        }));
    }
    let capability = async {
        let mut bytes = Vec::new();
        std::fs::File::open(app.node.data_dir.join("bootstrap.json"))?
            .take(4097)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 4096,
            "local bootstrap capability exceeds size limit"
        );
        let capability: BootstrapCapability = serde_json::from_slice(&bytes)?;
        app.node
            .store
            .lock()
            .await
            .verify_bootstrap_capability(&capability.capability)?;
        Ok::<_, anyhow::Error>(capability)
    }
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::CONFLICT,
            "local setup is unavailable; verify this Node's initialization before continuing"
                .into(),
        )
    })?;
    Ok(Json(LocalSetup::Uninitialized {
        invitation: BootstrapInvitation {
            url: origin.into(),
            peer_id,
            capability: capability.capability,
        },
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalJoinRequest {
    invitation: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalJoinAccept {
    request_id: String,
    review_hash: String,
}

async fn local_join_request(
    State(app): State<App>,
    Json(input): Json<LocalJoinRequest>,
) -> ApiResult<Json<JoinState>> {
    let invitation = Invitation::parse(&input.invitation).map_err(ApiError::bad)?;
    app.pairing
        .request(invitation, &input.name)
        .await
        .map(Json)
        .map_err(ApiError::bad)
}

async fn local_join_accept(
    State(app): State<App>,
    Json(input): Json<LocalJoinAccept>,
) -> ApiResult<Json<JoinState>> {
    app.pairing
        .accept(&input.request_id, &input.review_hash)
        .await
        .map(Json)
        .map_err(ApiError::bad)
}

async fn local_join_status(
    State(app): State<App>,
    Json(_): Json<LocalSetupRequest>,
) -> ApiResult<Json<JoinState>> {
    app.pairing.status().await.map(Json).map_err(ApiError::bad)
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
    let write = !matches!(
        input.tool_name.as_str(),
        "nodes" | "context" | "task_status" | "call_lookup"
    );
    let call_id = match input.tool_call_id {
        Some(id) => id,
        None if !write => uuid::Uuid::new_v4().to_string(),
        None => {
            return Err(ApiError::bad(
                "write tool callbacks require a stable toolCallId",
            ));
        }
    };
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

async fn network(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    websocket: WebSocketUpgrade,
) -> Response {
    let admission = match app.node.network.admit(peer.ip()) {
        Ok(admission) => admission,
        Err(error) => {
            let status = if error.rate() {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            return (status, error.code()).into_response();
        }
    };
    websocket
        .max_message_size(65535)
        .max_frame_size(65535)
        .on_upgrade(move |socket| async move {
            if let Err(error) = app.node.network.clone().accept(socket, admission).await {
                tracing::debug!(%error, "network connection closed");
            }
        })
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

mod web_assets {
    include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
}

async fn frontend(method: Method, uri: axum::http::Uri) -> Response {
    let path = uri.path();
    if path == "/api"
        || path.starts_with("/api/")
        || path == "/peer"
        || path.starts_with("/peer/")
        || path.starts_with("/sdk.v1.")
    {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if let Some(file) = web_assets::get(path) {
        let mut response = ([(header::CONTENT_TYPE, file.content_type)], file.body).into_response();
        if path.starts_with("/assets/") {
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
        }
        return response;
    }
    if path == "/assets"
        || path.starts_with("/assets/")
        || path == "/generated"
        || path.starts_with("/generated/")
        || path.rsplit('/').next().unwrap_or("").contains('.')
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        web_assets::get("/_shell.html")
            .expect("embedded SPA shell")
            .body,
    )
        .into_response()
}

async fn transport_javascript() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/transport.js"),
    )
}
async fn crypto_javascript() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/generated/vibeface_secure.js"),
    )
}
async fn crypto_wasm() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/wasm")],
        include_bytes!("../web/generated/vibeface_secure_bg.wasm").as_slice(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_and_host_allow_fixed_local_aliases_only() {
        let sites = [
            "http://192.168.0.99:8787",
            "http://localhost:8787",
            "http://127.0.0.1:8787",
            "http://0.0.0.0:8787",
            "http://[::1]:8787",
            "http://[::]:8787",
        ]
        .into_iter()
        .map(|origin| Site::parse(origin).unwrap())
        .collect::<Vec<_>>();

        for site in &sites {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, site.authority.parse().unwrap());
            headers.insert(header::ORIGIN, site.origin.parse().unwrap());
            assert!(check_site(&sites, &headers).is_ok());
        }

        let main = &sites[0];
        let alias = &sites[1];
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, main.authority.parse().unwrap());
        headers.insert(header::ORIGIN, alias.origin.parse().unwrap());
        assert!(check_site(&sites, &headers).is_err());
        headers.insert(header::ORIGIN, "https://hostile.invalid".parse().unwrap());
        assert!(check_site(&sites, &headers).is_err());
        headers.insert(header::ORIGIN, "http://127.0.0.1:8787".parse().unwrap());
        headers.insert(header::HOST, "hostile.invalid:8787".parse().unwrap());
        assert!(check_site(&sites, &headers).is_err());
        headers.insert(header::HOST, main.authority.parse().unwrap());
        headers.append(header::HOST, "hostile.invalid".parse().unwrap());
        assert!(check_site(&sites, &headers).is_err());
        assert!(Site::parse("https://example.com/path").is_err());
        assert!(Site::parse("https://user:secret@example.com").is_err());
    }

    #[test]
    fn insecure_http_sites_require_loopback_or_lan_opt_in() {
        let http = Site::parse("http://localhost:8787").unwrap();
        let https = Site::parse("https://node.example:8787").unwrap();
        assert!(http_site_allowed(
            &http,
            false,
            "127.0.0.1".parse().unwrap()
        ));
        assert!(http_site_allowed(&http, false, "::1".parse().unwrap()));
        assert!(!http_site_allowed(
            &http,
            false,
            "192.168.0.50".parse().unwrap()
        ));
        assert!(http_site_allowed(
            &http,
            true,
            "192.168.0.50".parse().unwrap()
        ));
        assert!(http_site_allowed(
            &https,
            false,
            "192.168.0.50".parse().unwrap()
        ));
    }

    #[test]
    fn local_setup_requires_a_literal_loopback_site_peer_and_exact_origin_without_forwarding() {
        let sites = [
            "http://localhost:4790",
            "http://127.0.0.1:4790",
            "http://[::1]:4790",
        ]
        .into_iter()
        .map(|origin| Site::parse(origin).unwrap())
        .collect::<Vec<_>>();
        let allowed = |headers: &HeaderMap, peer| {
            check_site(&sites, headers)
                .and_then(|site| check_local_setup(site, headers, peer))
                .is_ok()
        };
        for site in &sites {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, site.authority.parse().unwrap());
            headers.insert(header::ORIGIN, site.origin.parse().unwrap());
            assert!(allowed(&headers, "127.0.0.1".parse().unwrap()));
            assert!(allowed(&headers, "::1".parse().unwrap()));
            assert!(!allowed(&headers, "192.168.0.10".parse().unwrap()));
            assert!(!allowed(&headers, "203.0.113.10".parse().unwrap()));
            for name in [
                "forwarded",
                "x-forwarded-for",
                "x-forwarded-host",
                "x-forwarded-proto",
                "x-real-ip",
            ] {
                headers.insert(name, "127.0.0.1".parse().unwrap());
                assert!(!allowed(&headers, "127.0.0.1".parse().unwrap()));
                headers.remove(name);
            }
            headers.insert(header::ORIGIN, "http://LOCALHOST:4790".parse().unwrap());
            assert!(!allowed(&headers, "127.0.0.1".parse().unwrap()));
            headers.insert(header::ORIGIN, "https://hostile.invalid".parse().unwrap());
            assert!(!allowed(&headers, "127.0.0.1".parse().unwrap()));
            headers.remove(header::ORIGIN);
            assert!(!allowed(&headers, "127.0.0.1".parse().unwrap()));
            headers.insert(header::ORIGIN, site.origin.parse().unwrap());
            headers.insert(header::HOST, "hostile.invalid:4790".parse().unwrap());
            assert!(!allowed(&headers, "127.0.0.1".parse().unwrap()));
        }
        for origin in [
            "http://0.0.0.0:4790",
            "http://[::]:4790",
            "https://node.example",
            "http://localhost.example:4790",
            "http://localhost.:4790",
        ] {
            let site = Site::parse(origin).unwrap();
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, site.authority.parse().unwrap());
            headers.insert(header::ORIGIN, site.origin.parse().unwrap());
            assert!(check_local_setup(&site, &headers, "127.0.0.1".parse().unwrap()).is_err());
        }
    }

    #[tokio::test]
    async fn local_setup_endpoint_reuses_one_time_capability_and_stops_after_initialization()
    -> Result<()> {
        use crate::{
            config::{self, Config},
            crypto::LocalIdentity,
            identity::{Genesis, Member, NetworkState, NodeRole},
        };

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let origin = format!("http://{address}");
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("local-setup-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let mut config = Config::local(address, vec![], false);
        config.roles = vec![NodeRole::Relay];
        config.save(&dir)?;
        config::private_write(&dir.join("data-epoch.json"), br#"{"data_epoch":5}"#)?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        let store = crate::store::Store::open(dir.join("node.sqlite3"))?;
        let capability = store.bootstrap_capability()?;
        config::private_write(
            &dir.join("bootstrap.json"),
            &serde_json::to_vec(&capability)?,
        )?;
        drop(store);
        let node = Arc::new(Runtime::open(&dir).await?);
        let app = router(node.clone());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build()?;
        let endpoint = format!("{origin}/api/local/setup");
        let result = async {
            let request = || client.post(&endpoint).header(header::ORIGIN, &origin).json(&json!({}));
            let response = request().send().await?;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let first: Value = response.json().await?;
            assert_eq!(first["kind"], "uninitialized");
            assert_eq!(first["invitation"]["url"], origin);
            assert_eq!(first["invitation"]["peer_id"], node.identity.peer_id()?);
            assert_eq!(first["invitation"]["capability"], capability.capability);
            assert_eq!(request().send().await?.json::<Value>().await?, first);
            assert_eq!(client.get(&endpoint).header(header::ORIGIN, &origin).send().await?.status(), StatusCode::METHOD_NOT_ALLOWED);
            assert_eq!(client.post(&endpoint).header(header::ORIGIN, &origin).header(header::CONTENT_TYPE, "text/plain").body("{}").send().await?.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
            assert_eq!(client.post(&endpoint).json(&json!({})).send().await?.status(), StatusCode::FORBIDDEN);
            assert_eq!(request().header("x-forwarded-for", "127.0.0.1").send().await?.status(), StatusCode::FORBIDDEN);
            assert_eq!(client.post(&endpoint).header(header::ORIGIN, "https://hostile.invalid").json(&json!({})).send().await?.status(), StatusCode::FORBIDDEN);
            for (path, input) in [
                ("status", json!({})),
                ("request", json!({"invitation":"invalid","name":"Mac mini"})),
                ("accept", json!({"request_id":"request","review_hash":"00"})),
            ] {
                let endpoint = format!("{origin}/api/local/join/{path}");
                assert_eq!(client.post(&endpoint).json(&input).send().await?.status(), StatusCode::FORBIDDEN);
                assert_eq!(client.post(&endpoint).header(header::ORIGIN, &origin).header("x-forwarded-for", "127.0.0.1").json(&input).send().await?.status(), StatusCode::FORBIDDEN);
                assert_eq!(client.post(&endpoint).header(header::ORIGIN, "https://hostile.invalid").json(&input).send().await?.status(), StatusCode::FORBIDDEN);
                assert_eq!(client.post(&endpoint).header(header::ORIGIN, &origin).header(header::CONTENT_TYPE, "text/plain").body("{}").send().await?.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
                let response = client.post(&endpoint).header(header::ORIGIN, &origin).json(&input).send().await?;
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                if path == "status" { assert_eq!(response.json::<Value>().await?, json!({"kind":"idle"})); }
                else { assert_eq!(response.status(), StatusCode::BAD_REQUEST); }
            }
            let public_info: Value = client.get(format!("{origin}/api/info")).send().await?.json().await?;
            assert!(!public_info.to_string().contains(&capability.capability));
            assert!(public_info.get("invitation").is_none());
            let root = LocalIdentity::generate();
            let admin = LocalIdentity::generate();
            let state = NetworkState::new(Genesis::create(&root, vec![
                Member::client(&admin, "browser", true)?,
                Member::node(&node.identity, "node", node.config.roles.clone())?,
            ])?)?;
            node.store.lock().await.bootstrap_network(state, &capability.capability, &admin.peer_id()?)?;
            let initialized: Value = request().send().await?.json().await?;
            assert_eq!(initialized, json!({"kind":"initialized","entry":{"address":origin,"peer_id":node.identity.peer_id()?,"relay":false}}));
            assert!(!initialized.to_string().contains(&capability.capability));
            assert!(node.store.lock().await.verify_bootstrap_capability(&capability.capability).is_err());
            let local_origin = "http://127.0.0.1:4791";
            node.local_access.publish_listener(local_origin.into(), node.identity.peer_id()?)?;
            let launched: Value = request().send().await?.json().await?;
            assert_eq!(launched["entry"], initialized["entry"]);
            assert_eq!(launched["local_launch"]["url"], local_origin);
            assert_eq!(launched["local_launch"]["peer_id"], node.identity.peer_id()?);
            let token = launched["local_launch"]["token"].as_str().context("local token missing")?;
            assert!(token.len() == 64 && hex::decode(token).is_ok());
            let browser = LocalIdentity::generate();
            let network_id = node.membership_state().await?.genesis.network_id;
            let cookie = node.local_access.exchange(token, &node.identity.peer_id()?, &network_id, &browser.peer_id()?, &browser.public_key_hex()?)?;
            assert_eq!(node.local_access.session(&cookie, &node.identity.peer_id()?, &network_id)?.subject_id, browser.peer_id()?);
            assert!(node.local_access.exchange(token, &node.identity.peer_id()?, &network_id, &browser.peer_id()?, &browser.public_key_hex()?).is_err());
            node.local_access.set_enabled(false)?;
            assert_eq!(request().send().await?.json::<Value>().await?, initialized);
            assert!(node.local_access.session(&cookie, &node.identity.peer_id()?, &network_id).is_err());
            let joined: Value = client.post(format!("{origin}/api/local/join/status")).header(header::ORIGIN, &origin).json(&json!({})).send().await?.json().await?;
            assert_eq!(joined["kind"], "joined");
            assert_eq!(joined["peer_id"], node.identity.peer_id()?);
            Ok::<(), anyhow::Error>(())
        }.await;
        server.abort();
        let _ = server.await;
        drop(node);
        std::fs::remove_dir_all(dir)?;
        result
    }

    #[test]
    fn authorization_requires_one_exact_bearer_header() {
        let mut headers = HeaderMap::new();
        assert!(bearer(&headers).is_none());
        headers.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert_eq!(bearer(&headers), Some("secret"));
        headers.append(header::AUTHORIZATION, "Bearer second".parse().unwrap());
        assert!(bearer(&headers).is_none());
        assert!(validate_tool_call("send_task", "stable-call-id").is_ok());
        assert!(validate_tool_call("send_task", "").is_err());
    }
}
