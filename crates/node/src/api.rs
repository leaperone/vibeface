use crate::runtime::Runtime;
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State, WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
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
                router_with_sites(node, sites).into_make_service_with_connect_info::<SocketAddr>(),
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
    let app = App { sites, node };
    Router::new()
        .route("/transport.js", get(transport_javascript))
        .route("/generated/vibeface_secure.js", get(crypto_javascript))
        .route("/generated/vibeface_secure_bg.wasm", get(crypto_wasm))
        .route("/api/info", get(info))
        .route("/peer", get(peer))
        .route("/api/network/ws", get(network))
        .fallback(frontend)
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
        let is_tool = path == "/api/tools" || path == CALLBACK;
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

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let token = one_header(headers, header::AUTHORIZATION.as_str())?.strip_prefix("Bearer ")?;
    (!token.is_empty() && token.trim() == token).then_some(token)
}

async fn bearer_access(app: &App, headers: &HeaderMap) -> ApiResult<crate::model::AgentSession> {
    let token = bearer(headers).ok_or_else(ApiError::auth)?;
    app.node
        .authorize_bearer(token)
        .await
        .ok_or_else(ApiError::auth)
}

async fn info(State(app): State<App>) -> Json<Value> {
    Json(app.node.info())
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

async fn network(State(app): State<App>, websocket: WebSocketUpgrade) -> Response {
    websocket
        .max_message_size(65535)
        .max_frame_size(65535)
        .on_upgrade(move |socket| async move {
            if let Err(error) = app.node.network.clone().accept(socket).await {
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
