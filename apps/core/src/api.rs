use crate::{identity::local::BootstrapCapability, runtime::Runtime};
mod static_assets;
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
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};
use tokio::net::TcpListener;

use static_assets::{DEFAULT_CONTENT_SECURITY_POLICY, StaticAssets};

const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// The scoped CLI posts tool calls to this private path.
pub const TOOL_PATH: &str = "/sdk.v1.SdkCustomToolCallbackService/CallCustomTool";
pub(crate) const MAX_CLIENT_SOCKETS: usize = 256;
const MAX_DECISIONS: usize = 16;
const DECISIONS_PER_MINUTE: u32 = 12;
const MAX_DECISION_SOURCES: usize = 256;
const HOOKS: &str = "/api/hooks";
/// A hook whose event holds a request waits here for the answer.
const HOOKS_WAIT: &str = "/api/hooks/wait";
const TRIAGE: &str = "/api/triage";
const LOG: &str = "/api/log";
/// What a page of another allowed origin may read: the Noise socket and this Node's device ID.
const CROSS_ORIGIN: [&str; 2] = ["/api/client", "/api/info"];
const TLS_HANDSHAKE: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct App {
    node: Arc<Runtime>,
    sites: Vec<Site>,
    clients: Arc<tokio::sync::Semaphore>,
    decisions: Arc<DecisionLimit>,
    /// The `/api/client` connections each browser address holds, counted on `listen` alone.
    addresses: Option<crate::net::Shares>,
    assets: Option<Arc<StaticAssets>>,
    local: bool,
}

impl App {
    fn new(node: Arc<Runtime>, sites: Vec<Site>, assets: Option<Arc<StaticAssets>>) -> Self {
        Self {
            node,
            sites,
            clients: Arc::new(tokio::sync::Semaphore::new(MAX_CLIENT_SOCKETS)),
            decisions: Arc::new(DecisionLimit::default()),
            addresses: None,
            assets,
            local: false,
        }
    }
}

struct DecisionLimit {
    active: Arc<tokio::sync::Semaphore>,
    sources: Mutex<HashMap<IpAddr, (i64, u32)>>,
}

impl Default for DecisionLimit {
    fn default() -> Self {
        Self {
            active: Arc::new(tokio::sync::Semaphore::new(MAX_DECISIONS)),
            sources: Mutex::new(HashMap::new()),
        }
    }
}

impl DecisionLimit {
    fn enter(&self, source: IpAddr, now_ms: i64) -> ApiResult<tokio::sync::OwnedSemaphorePermit> {
        // Per client address as the relay and `listen` count them: an IPv6 /56, not each host.
        let source = crate::net::client_address(source);
        let denied = || {
            ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "too many decision requests".into(),
            )
        };
        let permit = self
            .active
            .clone()
            .try_acquire_owned()
            .map_err(|_| denied())?;
        let mut sources = self.sources.lock().map_err(|_| denied())?;
        sources.retain(|_, (start, _)| now_ms.saturating_sub(*start) < 60_000);
        if !sources.contains_key(&source) && sources.len() >= MAX_DECISION_SOURCES {
            return Err(denied());
        }
        let (_, used) = sources.entry(source).or_insert((now_ms, 0));
        if *used >= DECISIONS_PER_MINUTE {
            return Err(denied());
        }
        *used += 1;
        Ok(permit)
    }
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
        (
            self.0,
            Json(json!({"error": crate::logging::scrub(&self.1)})),
        )
            .into_response()
    }
}

pub async fn serve(node: Arc<Runtime>, app_dir: Option<PathBuf>) -> Result<()> {
    let assets = app_dir.map(StaticAssets::load).transpose()?.map(Arc::new);
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
    let tls = node
        .config
        .tls
        .then(|| TlsFiles::load(&node.data_dir))
        .transpose()?;
    if let Some(files) = &tls {
        tracing::info!(event = "tls.loaded", sha256 = %files.leaf, "serving the TLS certificate");
    }
    let listener = if node.config.web_enabled {
        Some(TcpListener::bind(node.config.listen).await?)
    } else {
        None
    };
    let local_listen = node.config.local_listen;
    let local_listener = TcpListener::bind(local_listen).await.with_context(|| {
        format!(
            "端口 {} 被占用，可用 --local-listen 更换",
            local_listen.port()
        )
    })?;
    let local_url = format!("http://{local_listen}");
    node.local_access
        .publish_listener(local_url.clone(), node.identity.device_id()?)?;
    let local = browser_router_with_assets(
        node.clone(),
        vec![Site::parse(&local_url)?],
        true,
        assets.clone(),
    );
    // Codex threads keep the tool URL they were created with, so the port is reused across
    // starts when free; a new port means new threads.
    let port_file = node.data_dir.join("tool-port");
    let previous = std::fs::read_to_string(&port_file)
        .ok()
        .and_then(|port| port.trim().parse::<u16>().ok());
    let tools = match previous {
        Some(port) => match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => listener,
            Err(_) => TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?,
        },
        None => TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?,
    };
    let tool_addr = tools.local_addr()?;
    if previous != Some(tool_addr.port()) {
        std::fs::write(&port_file, tool_addr.port().to_string())?;
    }
    let tool_url = format!("http://{tool_addr}");
    let internal = tool_router(node.clone(), Site::parse(&tool_url)?);
    node.tools_ready(tool_url)?;
    tokio::try_join!(
        async {
            let router = browser_router_with_assets(node.clone(), sites, false, assets.clone());
            match listener {
                Some(tcp) => serve_public(tcp, tls, router, PUBLIC).await,
                None => std::future::pending().await,
            }
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

/// `config::TLS_CERT`, a full chain, and `config::TLS_KEY`, as `init --tls` makes them or a
/// certificate client on the host installs and renews them. A change to either file is seen at
/// the next connection, so a renewed certificate is served without a restart.
struct TlsFiles {
    dir: PathBuf,
    loaded: Stamp,
    acceptor: tokio_rustls::TlsAcceptor,
    /// SHA-256 of the leaf certificate served.
    leaf: String,
    /// The files as they were when they last failed to load, and when that was.
    failed: Option<(Stamp, std::time::Instant)>,
}

/// Each file's modification time and length.
type Stamp = [(std::time::SystemTime, u64); 2];
/// An unchanged pair that failed is tried again this often: a new owner or mode, which a
/// renewal may set after writing, changes neither field of its stamp.
const TLS_RETRY: Duration = Duration::from_secs(5);

impl TlsFiles {
    fn load(dir: &Path) -> Result<Self> {
        let loaded = tls_stamp(dir)?;
        let (acceptor, leaf) = tls_acceptor(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
            loaded,
            acceptor,
            leaf,
            failed: None,
        })
    }

    /// The acceptor for the files as they are now. A pair that does not load, such as one caught
    /// between its certificate and its key being replaced, leaves the last good one in place.
    fn current(&mut self) -> tokio_rustls::TlsAcceptor {
        let Ok(now) = tls_stamp(&self.dir) else {
            return self.acceptor.clone();
        };
        let due = self
            .failed
            .is_none_or(|(stamp, at)| stamp != now || at.elapsed() >= TLS_RETRY);
        if now != self.loaded && due {
            match tls_acceptor(&self.dir) {
                Ok((acceptor, leaf)) => {
                    tracing::info!(event = "tls.reloaded", sha256 = %leaf, "serving a new TLS certificate");
                    (self.acceptor, self.leaf, self.loaded, self.failed) =
                        (acceptor, leaf, now, None);
                }
                Err(error) => {
                    if self.failed.is_none_or(|(stamp, _)| stamp != now) {
                        tracing::warn!(event = "tls.reload_failed", %error, "keeping the previous TLS certificate");
                    }
                    self.failed = Some((now, std::time::Instant::now()));
                }
            }
        }
        self.acceptor.clone()
    }
}

fn tls_stamp(dir: &Path) -> std::io::Result<Stamp> {
    let stamp = |name| {
        let file = std::fs::metadata(dir.join(name))?;
        Ok::<_, std::io::Error>((file.modified()?, file.len()))
    };
    Ok([
        stamp(crate::config::TLS_CERT)?,
        stamp(crate::config::TLS_KEY)?,
    ])
}

fn tls_acceptor(dir: &Path) -> Result<(tokio_rustls::TlsAcceptor, String)> {
    use sha2::{Digest as _, Sha256};
    use tokio_rustls::rustls::{
        self,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject as _},
    };
    let chain = CertificateDer::pem_file_iter(dir.join(crate::config::TLS_CERT))
        .and_then(Iterator::collect::<std::result::Result<Vec<_>, _>>)
        .context("invalid TLS certificate")?;
    let leaf = hex::encode(Sha256::digest(chain.first().context("no TLS certificate")?));
    let key = PrivateKeyDer::from_pem_file(dir.join(crate::config::TLS_KEY))
        .context("invalid TLS key")?;
    // Refuses a key that is not the leaf certificate's.
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(chain, key)?;
    Ok((tokio_rustls::TlsAcceptor::from(Arc::new(config)), leaf))
}

/// What `listen` admits. Anyone may reach it, so each connection holds a place until it closes,
/// or until the WebSocket it became does.
#[derive(Clone, Copy)]
struct Admission {
    connections: usize,
    /// Per client address, an IPv4 address or IPv6 /56 as the relay counts them.
    per_address: NonZeroUsize,
    /// How long a connection may take to send a request head, just connected or idle after its
    /// last response.
    request_head: Duration,
}

/// Room for the official forwarding limits (32 sessions, 192 queued, 8 per address) and a
/// browser's own HTTP connections, far below the descriptors the relay and discovery service in
/// this process also need.
const PUBLIC: Admission = Admission {
    connections: 1024,
    per_address: NonZeroUsize::new(32).unwrap(),
    request_head: Duration::from_secs(20),
};

/// Serves `listen`, over TLS when `tls` is set. A connection past either limit is closed unread.
async fn serve_public(
    tcp: TcpListener,
    mut tls: Option<TlsFiles>,
    router: Router,
    admission: Admission,
) -> std::io::Result<()> {
    let places = Arc::new(tokio::sync::Semaphore::new(admission.connections));
    let addresses = crate::net::Shares::default();
    loop {
        let (stream, peer) = match tcp.accept().await {
            Ok(accepted) => accepted,
            // Out of descriptors, say; retrying at once would spin.
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(place) = places.clone().try_acquire_owned() else {
            continue;
        };
        let Some(share) = addresses.take(peer.ip(), admission.per_address) else {
            continue;
        };
        let held = (place, share);
        let acceptor = tls.as_mut().map(TlsFiles::current);
        let router = router.clone();
        // Handshakes run apart, so a slow client holds up no other.
        tokio::spawn(async move {
            let Some(acceptor) = acceptor else {
                return serve_connection(stream, held, peer, router, admission).await;
            };
            let accepting = tokio::time::timeout(TLS_HANDSHAKE, acceptor.accept(stream));
            if let Ok(Ok(stream)) = accepting.await {
                serve_connection(stream, held, peer, router, admission).await;
            }
        });
    }
}

async fn serve_connection<I>(
    io: I,
    held: (tokio::sync::OwnedSemaphorePermit, crate::net::Share),
    peer: SocketAddr,
    router: Router,
    admission: Admission,
) where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |mut request: Request<hyper::body::Incoming>| {
        request.extensions_mut().insert(ConnectInfo(peer));
        tower_service::Service::call(&mut router.clone(), request)
    });
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(admission.request_head);
    let _ = http
        .serve_connection(
            hyper_util::rt::TokioIo::new(Admitted { io, _held: held }),
            service,
        )
        .with_upgrades()
        .await;
}

/// A connection and its admission, given back when the connection, or the WebSocket it became,
/// closes.
struct Admitted<I> {
    io: I,
    _held: (tokio::sync::OwnedSemaphorePermit, crate::net::Share),
}

impl<I: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Admitted<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<I: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Admitted<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

fn tool_router(node: Arc<Runtime>, site: Site) -> Router {
    let app = App::new(node, vec![site], None);
    Router::new()
        .route(TOOL_PATH, post(tool))
        .route("/mcp", post(mcp))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES / 2))
        .layer(middleware::from_fn_with_state(app.clone(), boundary))
        .with_state(app)
}

#[cfg(test)]
fn browser_router(node: Arc<Runtime>, sites: Vec<Site>, local: bool) -> Router {
    browser_router_with_assets(node, sites, local, None)
}

fn browser_router_with_assets(
    node: Arc<Runtime>,
    sites: Vec<Site>,
    local: bool,
    assets: Option<Arc<StaticAssets>>,
) -> Router {
    let mut app = App::new(node, sites, assets);
    app.local = local;
    // Anyone may reach `listen`; every connection to the local listener is this computer's.
    if !local {
        app.addresses = Some(Default::default());
    }
    let mut routes = Router::new();
    if app.node.config.web_enabled {
        routes = routes
            .route("/api/info", get(info))
            .fallback(frontend)
            .route("/api/local/setup", post(local_setup))
            .route("/api/client", get(client_gateway))
            .route("/api/decision", post(decision));
        if local {
            routes = routes
                .route("/api/local/info", get(local_info))
                .route("/api/local/launch", post(local_launch))
                .route("/api/local/exchange", post(local_exchange))
                .route("/api/local/ws", get(local_websocket));
        }
    }
    if local {
        routes = routes
            .route(HOOKS, post(hooks))
            .route(HOOKS_WAIT, post(wait_hook))
            .route(TRIAGE, post(triage))
            .route(LOG, post(log));
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
    .and_then(|()| {
        // Admission comes before maintenance or any discovery and dial work.
        if request.uri().path() == "/api/decision" && *request.method() == Method::POST {
            app.decisions
                .enter(
                    client_ip(
                        ip,
                        request.headers(),
                        &app.node.config.forwarding.trusted_proxies,
                    ),
                    crate::runtime::now_ms(),
                )
                .map(Some)
        } else {
            Ok(None)
        }
    }) {
        Ok(_decision) => {
            let work = if matches!(
                *request.method(),
                Method::GET | Method::HEAD | Method::OPTIONS
            ) {
                Ok(None)
            } else if request.uri().path() == HOOKS_WAIT {
                // A hook waits up to its request's deadline; that must not hold off an update.
                app.node.maintenance.enter().map(|_| None)
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
        app.assets.as_ref().map_or_else(
            || HeaderValue::from_static(DEFAULT_CONTENT_SECURITY_POLICY),
            |assets| assets.content_security_policy.clone(),
        ),
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
    let site = if CROSS_ORIGIN.contains(&path) && *method == Method::GET {
        // Cross-origin by design: this Node's browser origins and those of the personal
        // network's other Gateways, as their signed address records state them. A page such as
        // the official App, served elsewhere, reaches its access Node this way.
        let site = host_site(&app.sites, headers)?;
        if let Some(origin) = headers.get(header::ORIGIN) {
            let origin = one_header(headers, header::ORIGIN.as_str())
                .filter(|_| origin.to_str().is_ok())
                .ok_or_else(ApiError::forbidden)?;
            if !app
                .sites
                .iter()
                .any(|site| site.origin.eq_ignore_ascii_case(origin))
                && !app.node.member_gateway_origin(origin).await
            {
                return Err(ApiError::forbidden());
            }
        }
        site
    } else {
        check_site(&app.sites, headers)?
    };
    if !http_site_allowed(site, app.node.config.allow_insecure_lan, ip) {
        return Err(ApiError::forbidden());
    }
    if path == "/api/local/setup" {
        check_local_setup(site, headers, ip)?;
    }
    if path == HOOKS || path == HOOKS_WAIT || path == TRIAGE || path == LOG {
        check_hook(headers, ip)?;
        if app.local && !bearer(headers).is_some_and(|token| app.node.local_access.is_owner(token))
        {
            return Err(ApiError::auth());
        }
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
        let is_tool = path == TOOL_PATH
            || path == "/mcp"
            || path == "/api/local/launch"
            || path == "/api/decision";
        let bearer_call = (is_tool && bearer(headers).is_some())
            || path == HOOKS
            || path == HOOKS_WAIT
            || path == TRIAGE
            || path == LOG;
        if !bearer_call
            && !one_header(headers, header::ORIGIN.as_str())
                .is_some_and(|origin| site.origin.eq_ignore_ascii_case(origin))
        {
            return Err(ApiError::forbidden());
        }
    }
    Ok(())
}

fn host_site<'a>(sites: &'a [Site], headers: &HeaderMap) -> ApiResult<&'a Site> {
    let host = one_header(headers, header::HOST.as_str()).ok_or_else(ApiError::forbidden)?;
    sites
        .iter()
        .find(|site| host.eq_ignore_ascii_case(&site.authority))
        .ok_or_else(ApiError::forbidden)
}

fn check_site<'a>(sites: &'a [Site], headers: &HeaderMap) -> ApiResult<&'a Site> {
    let site = host_site(sites, headers)?;
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

/// Hooks and `vibeface triage` run as local processes: a loopback peer, and no `Origin`, so no web page can post.
fn check_hook(headers: &HeaderMap, peer: IpAddr) -> ApiResult<()> {
    if !peer.is_loopback()
        || headers.contains_key(header::ORIGIN)
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

/// The boundary already refused an `Origin` this Node does not allow, so any left may read it.
async fn info(State(app): State<App>, headers: HeaderMap) -> Response {
    let mut response = Json(app.node.info()).into_response();
    if let Some(origin) = headers.get(header::ORIGIN) {
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        headers.insert(header::VARY, HeaderValue::from_static("origin"));
    }
    response
}

async fn local_info(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let device_id = app.node.identity.device_id().map_err(ApiError::bad)?;
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
        local_cookie(&headers, &device_id).and_then(|cookie| {
            app.node
                .local_access
                .session(cookie, &device_id, network_id)
                .ok()
        })
    });
    Ok(Json(
        json!({"enabled":app.node.local_access.enabled(),"has_session":local_session.is_some(),"device_id":local_session.map(|session| session.device_id),"entry":{"address":app.node.local_access.origin().map_err(ApiError::bad)?,"device_id":device_id,"local":true}}),
    ))
}

async fn local_launch(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let capability = bearer(&headers).ok_or_else(ApiError::auth)?;
    let state = app.node.membership_state().await.map_err(ApiError::bad)?;
    let device_id = app.node.identity.device_id().map_err(ApiError::bad)?;
    let token = app
        .node
        .local_access
        .issue(capability, &device_id, &state.genesis.network_id)
        .map_err(|_| ApiError::auth())?;
    Ok(Json(
        json!({"url":app.node.local_access.origin().map_err(ApiError::bad)?,"device_id":device_id,"token":token}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalExchange {
    token: String,
    node_id: String,
    device_id: String,
}

async fn local_exchange(
    State(app): State<App>,
    Json(input): Json<LocalExchange>,
) -> ApiResult<Response> {
    let device_id = app.node.identity.device_id().map_err(ApiError::bad)?;
    if input.node_id != device_id {
        return Err(ApiError::auth());
    }
    let state = app.node.membership_state().await.map_err(ApiError::bad)?;
    let cookie = app
        .node
        .local_access
        .exchange(
            &input.token,
            &device_id,
            &state.genesis.network_id,
            &input.device_id,
        )
        .map_err(|_| ApiError::auth())?;
    let mut response = Json(json!({"entry":{"address":app.node.local_access.origin().map_err(ApiError::bad)?,"device_id":device_id,"local":true},"network_id":state.genesis.network_id})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{}={cookie}; HttpOnly; SameSite=Strict; Path=/api/local; Max-Age={}",
            crate::local_access::cookie_name(&device_id),
            crate::local_access::SESSION_SECONDS
        ))
        .map_err(ApiError::bad)?,
    );
    Ok(response)
}

fn local_cookie<'a>(headers: &'a HeaderMap, device_id: &str) -> Option<&'a str> {
    let name = crate::local_access::cookie_name(device_id);
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
    let device_id = app.node.identity.device_id().map_err(ApiError::bad)?;
    let cookie = local_cookie(&headers, &device_id)
        .ok_or_else(ApiError::auth)?
        .to_owned();
    let state = app.node.membership_state().await.map_err(ApiError::bad)?;
    app.node
        .local_access
        .session(&cookie, &device_id, &state.genesis.network_id)
        .map_err(|_| ApiError::auth())?;
    Ok(websocket
        .max_message_size(65535)
        .max_frame_size(65535)
        .on_upgrade(move |socket| async move {
            if let Err(error) = crate::client::serve_local(
                app.node,
                crate::client::ClientSocket::server(socket),
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
    OwnerRequired {
        initialized: bool,
    },
    Uninitialized {
        invitation: BootstrapInvitation,
    },
    Initialized {
        entry: LocalEntry,
        /// Lets a browser that never joined show this computer as removed instead of not joined.
        removed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        local_launch: Option<LocalLaunch>,
    },
}

#[derive(Serialize)]
struct LocalLaunch {
    url: String,
    device_id: String,
    token: String,
}

#[derive(Serialize)]
struct LocalEntry {
    address: String,
    device_id: String,
}

#[derive(Serialize)]
struct BootstrapInvitation {
    url: String,
    device_id: String,
    capability: String,
}

async fn local_setup(
    State(app): State<App>,
    headers: HeaderMap,
    Json(_): Json<LocalSetupRequest>,
) -> ApiResult<Json<LocalSetup>> {
    // A public node's page opens other people's join links; its operator sets it up from
    // its access URL instead.
    if app.node.config.forwarding.any {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "a public node has no local setup".into(),
        ));
    }
    let origin = one_header(&headers, header::ORIGIN.as_str()).ok_or_else(ApiError::forbidden)?;
    let device_id = app.node.identity.device_id().map_err(ApiError::bad)?;
    let initialized = app.node.has_membership().await.map_err(ApiError::bad)?;
    let owner =
        app.local && bearer(&headers).is_some_and(|token| app.node.local_access.is_owner(token));
    if initialized {
        let state = app.node.membership_state().await.map_err(ApiError::bad)?;
        let removed = state
            .project()
            .map_err(ApiError::bad)?
            .revocation(&device_id)
            .is_some();
        let local_launch = if owner && app.node.local_access.enabled() {
            match app.node.local_access.origin() {
                Ok(url) => {
                    let token = app
                        .node
                        .local_access
                        .issue(
                            bearer(&headers).ok_or_else(ApiError::auth)?,
                            &device_id,
                            &state.genesis.network_id,
                        )
                        .map_err(ApiError::bad)?;
                    Some(LocalLaunch {
                        url,
                        device_id: device_id.clone(),
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
                device_id,
            },
            removed,
            local_launch,
        }));
    }
    if !owner {
        return Ok(Json(LocalSetup::OwnerRequired { initialized }));
    }
    let capability = async {
        let bytes = vibeface_bridge::platform::privatefs::read_private_bounded(
            &app.node.data_dir.join("bootstrap.json"),
            4096,
            "the bootstrap capability",
        )?;
        let capability: BootstrapCapability = serde_json::from_slice(&bytes)?;
        let verified_capability = capability.capability.clone();
        app.node
            .store
            .call("local_bootstrap_capability", move |store| {
                store.verify_bootstrap_capability(&verified_capability)
            })
            .await?;
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
            device_id,
            capability: capability.capability,
        },
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallback {
    tool_name: String,
    args: Value,
    agent_id: String,
}

async fn tool(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<ToolCallback>,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return ApiError::auth().into_response();
    };
    let Some(access) = app.node.tool_access(token, &input.agent_id).await else {
        return ApiError::auth().into_response();
    };
    if !input.args.is_object() || input.agent_id.is_empty() || input.agent_id.len() > 256 {
        return ApiError::bad("args must be an object and agentId must be nonempty and bounded")
            .into_response();
    }
    match app.node.tool(access, &input.tool_name, input.args).await {
        Ok(result) => Json(json!({"result": result})).into_response(),
        Err(error) => match error.downcast_ref::<crate::tools::ToolFailure>() {
            Some(error) => (StatusCode::BAD_REQUEST, Json(error.response.clone())).into_response(),
            None => ApiError::bad(error).into_response(),
        },
    }
}

/// Stateless Streamable HTTP MCP, on the private tool listener only. Every request uses the
/// current attempt's bearer; loading a Cursor session never restores a previous bearer.
async fn mcp(State(app): State<App>, headers: HeaderMap, Json(input): Json<Value>) -> Response {
    let Some(token) = bearer(&headers) else {
        return ApiError::auth().into_response();
    };
    let Some(access) = app.node.tool_access(token, "mcp").await else {
        return ApiError::auth().into_response();
    };
    let id = input.get("id").cloned().unwrap_or(Value::Null);
    let error = |code, message: &str| {
        Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
            .into_response()
    };
    if input["jsonrpc"] != "2.0"
        || !input.is_object()
        || !matches!(id, Value::Null | Value::String(_) | Value::Number(_))
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
            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"vibeface","version":env!("CARGO_PKG_VERSION")}})
        }
        "ping" => json!({}),
        "tools/list" => {
            let tools: Vec<Value> = crate::providers::Provider::Cursor.tools_for(crate::scheduling::TriggerKind::Human)
                .into_iter().map(|(name,tool)| json!({"name":name,"description":tool.description,"inputSchema":tool.input_schema})).collect();
            json!({"tools":tools})
        }
        "tools/call" => {
            let Some(name) = input["params"]["name"].as_str() else {
                return error(-32602, "A tool name is required");
            };
            let args = input["params"]
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !args.is_object() {
                return error(-32602, "Tool arguments must be an object");
            }
            match app.node.tool(access, name, args).await {
                Ok(result) => {
                    json!({"content":[{"type":"text","text":result.to_string()}],"isError":false})
                }
                Err(error) => {
                    let result = error
                        .downcast_ref::<crate::tools::ToolFailure>()
                        .map(|error| error.response.clone())
                        .unwrap_or_else(|| json!({"error":"Node tool call failed"}));
                    json!({"content":[{"type":"text","text":result.to_string()}],"isError":true})
                }
            }
        }
        _ => return error(-32601, "Method not found"),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}

async fn hooks(State(app): State<App>, Json(mut input): Json<Value>) -> ApiResult<Json<Value>> {
    let schema = input
        .as_object_mut()
        .ok_or_else(|| ApiError::bad("hook event must be an object"))?
        .remove("_request")
        .map(serde_json::from_value)
        .transpose()
        .map_err(ApiError::bad)?;
    let event = serde_json::from_value(input).map_err(ApiError::bad)?;
    Ok(Json(
        app.node
            .ingest_hook_event(event, schema)
            .await
            .map_err(ApiError::bad)?,
    ))
}

async fn wait_hook(State(app): State<App>, Json(input): Json<Value>) -> ApiResult<Json<Value>> {
    let request_id = input["request_id"]
        .as_str()
        .ok_or_else(|| ApiError::bad("request_id is required"))?;
    Ok(Json(app.node.wait_request(request_id).await))
}

/// The coordination Agent's triage answers, from `vibeface triage` on this device.
async fn triage(State(app): State<App>, Json(input): Json<Value>) -> ApiResult<Json<Value>> {
    Ok(Json(
        app.node
            .triage_command(input)
            .await
            .map_err(ApiError::bad)?,
    ))
}

/// Records from `vibeface log` on this device: the coordination Agent or a script.
async fn log(State(app): State<App>, Json(input): Json<Value>) -> ApiResult<Json<Value>> {
    use crate::logging::Source;
    let source: Source = serde_json::from_value(input["source"].clone())
        .ok()
        .filter(|source| matches!(source, Source::Agent | Source::Cli))
        .ok_or_else(|| ApiError::bad("source must be agent or cli"))?;
    let accepted = crate::logging::submit(
        &app.node.log_intake,
        &format!("local-{source:?}"),
        source,
        &input["records"],
        crate::runtime::now_ms(),
    )
    .map_err(ApiError::bad)?;
    Ok(Json(json!({"accepted":accepted})))
}

async fn decision(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let token = bearer(&headers).ok_or_else(ApiError::auth)?;
    app.node
        .decision_http(token, body)
        .await
        .map(Json)
        .map_err(|error| {
            let text = format!("{error:#}");
            if text.contains("invalid decision token") {
                ApiError::auth()
            } else {
                ApiError::bad(text)
            }
        })
}

async fn client_gateway(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    let forwarding = &app.node.config.forwarding;
    // Taken before the shared sockets, so one address cannot hold them all.
    let share = match (&app.addresses, forwarding.per_ip) {
        (Some(addresses), Some(limit)) => {
            let client = client_ip(peer.ip(), &headers, &forwarding.trusted_proxies);
            let Some(share) = addresses.take(client, limit) else {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    "this address holds too many client connections",
                )
                    .into_response();
            };
            Some(share)
        }
        _ => None,
    };
    let Ok(permit) = app.clients.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many client connections",
        )
            .into_response();
    };
    websocket
        .max_message_size(65535)
        .max_frame_size(65535)
        .on_upgrade(move |socket| async move {
            let socket = crate::client::ClientSocket::server(socket);
            if let Err(error) = crate::client::serve_gateway(app.node, socket).await {
                tracing::debug!(%error, "client connection closed");
            }
            drop((permit, share));
        })
}

/// The browser's address: what a trusted proxy's single `X-Real-IP` names, or else the peer.
fn client_ip(peer: IpAddr, headers: &HeaderMap, trusted: &[IpAddr]) -> IpAddr {
    let peer = peer.to_canonical();
    if !trusted.iter().any(|proxy| proxy.to_canonical() == peer) {
        return peer;
    }
    one_header(headers, "x-real-ip")
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(peer)
}

async fn frontend(State(app): State<App>, method: Method, uri: axum::http::Uri) -> Response {
    let path = uri.path();
    if path == "/api" || path.starts_with("/api/") || path.starts_with("/sdk.v1.") {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if static_assets::StaticAssets::is_unsafe_path(path)
        || path
            .split('/')
            .any(|segment| segment.starts_with('.') || matches!(segment, "_headers" | "_redirects"))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(assets) = app.assets.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if path == "/" {
        return static_response(assets.shell_file(), "no-store");
    }
    if let Some(file) = assets.read(path) {
        return static_response(file, static_cache_control(path));
    }
    if path == "/assets"
        || path.starts_with("/assets/")
        || path == "/generated"
        || path.starts_with("/generated/")
        || path.rsplit('/').next().unwrap_or("").contains('.')
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    static_response(assets.shell_file(), "no-store")
}

fn static_cache_control(path: &str) -> &'static str {
    if path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else if matches!(
        path,
        "/" | "/_shell.html" | "/index.html" | "/vibeface_secure.js" | "/vibeface_secure_bg.wasm"
    ) {
        "no-store"
    } else {
        "no-cache"
    }
}

fn static_response(file: static_assets::StaticFile, cache: &'static str) -> Response {
    let mut response = ([(header::CONTENT_TYPE, file.content_type)], file.body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::test_support::{access as test_access, initialize_node};

    #[test]
    fn a_renewed_certificate_is_served_without_a_restart() -> Result<()> {
        use crate::config::{TLS_CERT, TLS_KEY};
        let dir = std::env::temp_dir().join(format!("vibeface-tls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir)?;
        let issue = |host: &str| {
            let made = rcgen::generate_simple_self_signed(vec![host.into()]).unwrap();
            (made.cert.pem(), made.signing_key.serialize_pem())
        };
        let write = |(cert, key): &(String, String)| {
            std::fs::write(dir.join(TLS_CERT), cert)?;
            std::fs::write(dir.join(TLS_KEY), key)
        };
        let fingerprint = |pem: &str| {
            use tokio_rustls::rustls::pki_types::{CertificateDer, pem::PemObject as _};
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(
                CertificateDer::from_pem_slice(pem.as_bytes()).unwrap(),
            ))
        };
        let first = issue("203.0.113.7");
        write(&first)?;
        let mut files = TlsFiles::load(&dir)?;
        assert_eq!(files.leaf, fingerprint(&first.0));
        // A chain: the leaf, then what signed it, as a certificate client installs a full chain.
        let renewed = issue("203.0.113.7");
        let chained = (format!("{}{}", renewed.0, first.0), renewed.1.clone());
        write(&chained)?;
        files.current();
        assert_eq!(files.leaf, fingerprint(&renewed.0));
        // Caught between the certificate and its key: the last good pair stays.
        let next = issue("203.0.113.7");
        std::fs::write(dir.join(TLS_CERT), &next.0)?;
        files.current();
        assert_eq!(files.leaf, fingerprint(&renewed.0));
        assert!(files.failed.is_some());
        std::fs::write(dir.join(TLS_KEY), &next.1)?;
        files.current();
        assert_eq!(files.leaf, fingerprint(&next.0));
        assert!(files.failed.is_none());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// `listen` closes a connection past its address's share unread, and one that sends no
    /// request head in time, just connected or idle after a response; a WebSocket stays open.
    #[tokio::test]
    async fn the_public_listener_bounds_each_address_and_silent_connections() -> Result<()> {
        use futures_util::{SinkExt as _, StreamExt as _};
        use tokio::{
            io::{AsyncReadExt as _, AsyncWriteExt as _},
            net::TcpStream,
        };
        use tokio_tungstenite::tungstenite::Message;
        let router =
            Router::new()
                .route(
                    "/",
                    get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move {
                        peer.ip().to_string()
                    }),
                )
                .route(
                    "/ws",
                    get(|upgrade: WebSocketUpgrade| async move {
                        upgrade.on_upgrade(|mut socket| async move {
                            while let Some(Ok(message)) = socket.recv().await {
                                if socket.send(message).await.is_err() {
                                    break;
                                }
                            }
                        })
                    }),
                );
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = tcp.local_addr()?;
        let head = Duration::from_millis(300);
        let admission = Admission {
            connections: 16,
            per_address: NonZeroUsize::new(2).unwrap(),
            request_head: head,
        };
        let server = tokio::spawn(serve_public(tcp, None, router, admission));
        // What the connection sends until it closes, or None while it stays open.
        let until_closed = async |stream: &mut TcpStream| {
            let (mut sent, mut buf) = (Vec::new(), [0; 1024]);
            loop {
                match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await {
                    Ok(Ok(0) | Err(_)) => return Some(sent),
                    Ok(Ok(n)) => sent.extend_from_slice(&buf[..n]),
                    Err(_) => return None,
                }
            }
        };
        let request = b"GET / HTTP/1.1\r\nHost: node\r\n\r\n";

        let (mut socket, _) = tokio_tungstenite::client_async(
            format!("ws://{addr}/ws"),
            TcpStream::connect(addr).await?,
        )
        .await?;
        let mut silent = TcpStream::connect(addr).await?;
        let mut third = TcpStream::connect(addr).await?;
        let _ = third.write_all(request).await;
        assert_eq!(
            until_closed(&mut third).await,
            Some(vec![]),
            "a third connection from one address is closed unread"
        );
        let started = std::time::Instant::now();
        assert!(until_closed(&mut silent).await.is_some());
        assert!(
            started.elapsed() < head * 4,
            "a silent connection is closed"
        );

        tokio::time::sleep(head * 2).await;
        socket.send(Message::text("still here")).await?;
        assert_eq!(
            socket.next().await.context("echo")??,
            Message::text("still here"),
            "a WebSocket outlives the request head timeout"
        );

        let mut idle = TcpStream::connect(addr).await?;
        idle.write_all(request).await?;
        let mut answer = Vec::new();
        while !answer.ends_with(b"127.0.0.1") {
            let mut buf = [0; 1024];
            let n = tokio::time::timeout(Duration::from_secs(2), idle.read(&mut buf)).await??;
            ensure!(n > 0, "closed before answering");
            answer.extend_from_slice(&buf[..n]);
        }
        assert!(answer.starts_with(b"HTTP/1.1 200 OK"));
        assert_eq!(
            until_closed(&mut idle).await,
            Some(vec![]),
            "a kept-alive connection that asks nothing more is closed"
        );
        drop(socket);
        server.abort();
        Ok(())
    }

    #[test]
    fn only_a_trusted_proxy_names_the_client_address() {
        let headers = |values: &[&str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append("x-real-ip", value.parse().unwrap());
            }
            headers
        };
        let proxy: IpAddr = "198.51.100.1".parse().unwrap();
        let named = headers(&["203.0.113.9"]);
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        assert_eq!(client_ip(proxy, &named, &[proxy]), ip("203.0.113.9"));
        assert_eq!(
            client_ip(ip("::ffff:198.51.100.1"), &named, &[proxy]),
            ip("203.0.113.9")
        );
        assert_eq!(
            client_ip(ip("192.0.2.4"), &named, &[proxy]),
            ip("192.0.2.4")
        );
        let doubled = headers(&["203.0.113.9", "203.0.113.10"]);
        assert_eq!(client_ip(proxy, &doubled, &[proxy]), proxy);
        assert_eq!(client_ip(proxy, &headers(&["nonsense"]), &[proxy]), proxy);
    }

    #[tokio::test]
    async fn mcp_reuses_the_scoped_write_journal_and_revokes_access() -> Result<()> {
        use crate::runtime::{test_bot as bot, test_node as node, test_start_reply as start_reply};
        let (node, admin, dir) = node().await?;
        bot(&node, &admin, "c", "Ops", &[]).await?;
        let network = node
            .store
            .call("fixture_network", |store| store.network())
            .await?
            .context("network")?;
        let (trigger, attempt) = start_reply(&node, &admin, "mcp-trigger").await?;
        node.register_manager_attempt_fixture(
            "mcp-current-attempt".into(),
            test_access(network.clone(), false, Some("c".into())),
            &attempt,
            &trigger,
            crate::providers::Provider::Cursor,
        )
        .await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let app = tool_router(node.clone(), Site::parse(&origin)?);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        });
        let client = reqwest::Client::builder().no_proxy().build()?;
        let url = format!("{origin}/mcp");
        let result=async {
            let call=|input:Value|client.post(&url).bearer_auth("mcp-current-attempt").json(&input).send();
            let initialize:Value=call(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}})).await?.json().await?;
            assert_eq!(initialize["result"]["capabilities"]["tools"]["listChanged"],false);
            let request=json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tool_call","arguments":{"tool":"create_bot","arguments":{"name":"Docs"},"call_ref":"one-intent"}}});
            let first:Value=call(request.clone()).await?.json().await?;
            assert_eq!(first["result"]["isError"],false);
            let basis={ let network = network.clone(); node.store.call("fixture_version_vector", move |store| store.version_vector(&network)).await? };
            let replay:Value=call(request).await?.json().await?;
            assert_eq!(first,replay);
            assert_eq!(basis,{ let network = network.clone(); node.store.call("fixture_version_vector", move |store| store.version_vector(&network)).await? });
            let missing_ref:Value=call(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"create_bot","arguments":{"name":"Duplicate"}}})).await?.json().await?;
            assert_eq!(missing_ref["result"]["isError"],true);
            assert_eq!(basis,{ let network = network.clone(); node.store.call("fixture_version_vector", move |store| store.version_vector(&network)).await? });
            let mut wrong=node.agent_tokens.lock().await.get("mcp-current-attempt").cloned().context("token")?;
            wrong.network_id="another-personal-network".into();
            node.agent_tokens.lock().await.insert("wrong-network".into(),wrong);
            let wrong:Value=client.post(&url).bearer_auth("wrong-network").json(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"context","arguments":{}}})).send().await?.json().await?;
            assert_eq!(wrong["result"]["isError"],true);
            node.agent_tokens.lock().await.remove("mcp-current-attempt");
            assert_eq!(call(json!({"jsonrpc":"2.0","id":5,"method":"tools/list"})).await?.status(),StatusCode::UNAUTHORIZED);
            Ok::<_,anyhow::Error>(())
        }.await;
        server.abort();
        let _ = server.await;
        node.shutdown().await;
        drop(node);
        std::fs::remove_dir_all(dir)?;
        result
    }

    #[test]
    fn decision_admission_bounds_concurrency_sources_and_rate_without_leaking_slots() -> Result<()>
    {
        let limit = DecisionLimit::default();
        let mut permits = Vec::new();
        for source in 0..MAX_DECISIONS {
            permits.push(
                limit
                    .enter(IpAddr::V4(Ipv4Addr::new(192, 0, 2, source as u8)), 0)
                    .unwrap(),
            );
        }
        assert_eq!(
            limit.enter("198.51.100.1".parse()?, 0).unwrap_err().0,
            StatusCode::TOO_MANY_REQUESTS
        );
        drop(permits);
        let source = "203.0.113.1".parse()?;
        for _ in 0..DECISIONS_PER_MINUTE {
            drop(limit.enter(source, 0).unwrap());
        }
        assert_eq!(
            limit.enter(source, 0).unwrap_err().0,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(limit.active.available_permits(), MAX_DECISIONS);
        drop(limit.enter(source, 60_000).unwrap());
        for source in 0..MAX_DECISION_SOURCES - 1 {
            drop(
                limit
                    .enter(IpAddr::V4(Ipv4Addr::new(198, 51, source as u8, 1)), 60_000)
                    .unwrap(),
            );
        }
        assert!(limit.sources.lock().unwrap().len() <= MAX_DECISION_SOURCES);
        assert_eq!(
            limit.enter("203.0.113.2".parse()?, 60_000).unwrap_err().0,
            StatusCode::TOO_MANY_REQUESTS
        );
        drop(limit.enter("203.0.113.2".parse()?, 120_000).unwrap());
        // IPv6 sources count per /56, as the relay and `listen` do.
        let block = |host: u16| -> IpAddr { format!("2001:db8:0:{host:x}::1").parse().unwrap() };
        for _ in 0..DECISIONS_PER_MINUTE {
            drop(limit.enter(block(0), 180_000).unwrap());
        }
        assert_eq!(
            limit.enter(block(0xff), 180_000).unwrap_err().0,
            StatusCode::TOO_MANY_REQUESTS,
            "another host of the same /56 shares its budget"
        );
        drop(limit.enter(block(0x100), 180_000).unwrap());
        Ok(())
    }

    #[test]
    fn static_cache_policy_keeps_shell_crypto_and_assets_distinct() {
        for path in [
            "/",
            "/_shell.html",
            "/index.html",
            "/vibeface_secure.js",
            "/vibeface_secure_bg.wasm",
        ] {
            assert_eq!(
                static_cache_control(path),
                "no-store",
                "{path} must not be cached"
            );
        }
        for path in ["/sw.js", "/manifest.webmanifest", "/icon.svg"] {
            assert_eq!(static_cache_control(path), "no-cache");
        }
        assert_eq!(
            static_cache_control("/assets/chunk-123.js"),
            "public, max-age=31536000, immutable"
        );
    }

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
            identity::{Genesis, GovernanceEvent, Member, NetworkState, Revocation},
        };

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let origin = format!("http://{address}");
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("local-setup-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let config = Config::local(address, vec![], false);
        initialize_node(&dir, &config, &LocalIdentity::generate())?;
        let store = crate::store::Store::open(dir.join("node.sqlite3"))?;
        let capability = store.bootstrap_capability()?;
        config::private_write(
            &dir.join("bootstrap.json"),
            &serde_json::to_vec(&capability)?,
        )?;
        drop(store);
        let node = Arc::new(Runtime::open(&dir).await?);
        node.local_access
            .publish_listener(origin.clone(), node.identity.device_id()?)?;
        let launcher = crate::local_access::Launcher::read(&dir)?;
        let app = browser_router(node.clone(), vec![Site::parse(&origin)?], true);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let public_listener = TcpListener::bind("127.0.0.1:0").await?;
        let public_origin = format!("http://{}", public_listener.local_addr()?);
        let public_app = browser_router(node.clone(), vec![Site::parse(&public_origin)?], false);
        let public_server = tokio::spawn(async move {
            axum::serve(
                public_listener,
                public_app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build()?;
        let endpoint = format!("{origin}/api/local/setup");
        let result = async {
            let request = || client.post(&endpoint).header(header::ORIGIN, &origin).bearer_auth(&launcher.capability).json(&json!({}));
            let anonymous: Value = client.post(&endpoint).header(header::ORIGIN, &origin).json(&json!({})).send().await?.json().await?;
            assert_eq!(anonymous, json!({"kind":"owner_required","initialized":false}));
            for owner in [None, Some(&launcher.capability)] {
                let mut proxy = client.post(format!("{public_origin}/api/local/setup")).header(header::ORIGIN, &public_origin).json(&json!({}));
                if let Some(owner) = owner { proxy = proxy.bearer_auth(owner); }
                assert_eq!(proxy.send().await?.json::<Value>().await?, anonymous,
                    "a public listener never issues owner credentials, including hidden-source loopback proxies");
            }
            let response = request().send().await?;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let first: Value = response.json().await?;
            assert_eq!(first["kind"], "uninitialized");
            assert_eq!(first["invitation"]["url"], origin);
            assert_eq!(first["invitation"]["device_id"], node.identity.device_id()?);
            assert_eq!(first["invitation"]["capability"], capability.capability);
            assert_eq!(request().send().await?.json::<Value>().await?, first);
            assert_eq!(client.get(&endpoint).header(header::ORIGIN, &origin).send().await?.status(), StatusCode::METHOD_NOT_ALLOWED);
            assert_eq!(client.post(&endpoint).header(header::ORIGIN, &origin).header(header::CONTENT_TYPE, "text/plain").body("{}").send().await?.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
            assert_eq!(client.post(&endpoint).json(&json!({})).send().await?.status(), StatusCode::FORBIDDEN);
            assert_eq!(request().header("x-forwarded-for", "127.0.0.1").send().await?.status(), StatusCode::FORBIDDEN);
            assert_eq!(client.post(&endpoint).header(header::ORIGIN, "https://hostile.invalid").json(&json!({})).send().await?.status(), StatusCode::FORBIDDEN);
            let removed = client.post(format!("{origin}/api/local/join/status")).header(header::ORIGIN, &origin).json(&json!({})).send().await?;
            assert_eq!(removed.status(), StatusCode::NOT_FOUND);
            let public_info: Value = client.get(format!("{origin}/api/info")).send().await?.json().await?;
            assert!(!public_info.to_string().contains(&capability.capability));
            assert!(public_info.get("invitation").is_none());
            let root = LocalIdentity::generate();
            let admin = LocalIdentity::generate();
            let state = NetworkState::new(Genesis::create(
                &root,
                Member::client(&admin, "browser", true)?,
                Member::node(&node.identity, "node")?,
            )?)?;
            let bootstrap_capability = capability.capability.clone();
            let admin = admin.device_id()?;
            node.store.call("fixture_bootstrap", move |store| store.bootstrap_network(state, &bootstrap_capability, &admin)).await?;
            node.local_access.set_enabled(false)?;
            let initialized: Value = request().send().await?.json().await?;
            assert_eq!(initialized, json!({"kind":"initialized","entry":{"address":origin,"device_id":node.identity.device_id()?},"removed":false}));
            assert!(!initialized.to_string().contains(&capability.capability));
            assert!({ let capability = capability.capability.clone(); node.store.call("fixture_consumed_capability", move |store| store.verify_bootstrap_capability(&capability)).await.is_err() });
            let local_origin = &origin;
            node.local_access.set_enabled(true)?;
            let launched: Value = request().send().await?.json().await?;
            assert_eq!(launched["entry"], initialized["entry"]);
            assert_eq!(launched["local_launch"]["url"], *local_origin);
            assert_eq!(launched["local_launch"]["device_id"], node.identity.device_id()?);
            let token = launched["local_launch"]["token"].as_str().context("local token missing")?;
            assert!(token.len() == 64 && hex::decode(token).is_ok());
            let browser = LocalIdentity::generate();
            let network_id = node.membership_state().await?.genesis.network_id;
            let cookie = node.local_access.exchange(token, &node.identity.device_id()?, &network_id, &browser.device_id()?)?;
            assert_eq!(node.local_access.session(&cookie, &node.identity.device_id()?, &network_id)?.device_id, browser.device_id()?);
            assert!(node.local_access.exchange(token, &node.identity.device_id()?, &network_id, &browser.device_id()?).is_err());
            node.local_access.set_enabled(false)?;
            assert_eq!(request().send().await?.json::<Value>().await?, initialized);
            assert!(node.local_access.session(&cookie, &node.identity.device_id()?, &network_id).is_err());
            let mut removed = node.membership_state().await?;
            removed.append(GovernanceEvent::revoke(&admin, &removed.project()?, Revocation::new(&node.identity.device_id()?, None)?)?)?;
            node.install_membership(removed).await?;
            let after_removal: Value = request().send().await?.json().await?;
            assert_eq!(after_removal["kind"], "initialized");
            assert_eq!(after_removal["removed"], true);
            Ok::<(), anyhow::Error>(())
        }.await;
        server.abort();
        let _ = server.await;
        public_server.abort();
        let _ = public_server.await;
        node.shutdown().await;
        drop(node);
        std::fs::remove_dir_all(dir)?;
        result
    }

    #[tokio::test]
    async fn hook_routes_take_only_local_processes_on_the_local_listener() -> Result<()> {
        use crate::config::{self, Config};
        for web_enabled in [true, false] {
            let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.artifacts")
                .join(format!("hook-boundary-{}", uuid::Uuid::new_v4()));
            config::private_dir(&dir)?;
            Config::local("127.0.0.1:0".parse()?, vec![], false).save(&dir)?;
            config::private_write(
                &dir.join("data-epoch.json"),
                &crate::config::data_epoch_marker(),
            )?;
            config::private_write(
                &dir.join("identity.json"),
                &serde_json::to_vec(&crate::crypto::LocalIdentity::generate())?,
            )?;
            let mut runtime = Runtime::open(&dir).await?;
            runtime.config.web_enabled = web_enabled;
            let node = Arc::new(runtime);
            assert_eq!(node.gateway_url().is_some(), web_enabled);
            let mut origins = vec![];
            let mut servers = vec![];
            for local in [true, false] {
                let listener = TcpListener::bind("127.0.0.1:0").await?;
                let origin = format!("http://{}", listener.local_addr()?);
                if local {
                    node.local_access
                        .publish_listener(origin.clone(), node.identity.device_id()?)?;
                }
                let app = browser_router(node.clone(), vec![Site::parse(&origin)?], local);
                origins.push(origin);
                servers.push(tokio::spawn(async move {
                    axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<SocketAddr>(),
                    )
                    .await
                    .unwrap();
                }));
            }
            let client = reqwest::Client::builder().no_proxy().build()?;
            let launcher = crate::local_access::Launcher::read(&dir)?;
            node.local_access.set_enabled(false)?;
            assert!(node.local_access.is_owner(&launcher.capability));
            assert!(
                node.local_access
                    .issue(&launcher.capability, &launcher.node_id, "network")
                    .is_err()
            );
            let result = async {
                if !web_enabled {
                    for origin in &origins {
                        for path in [
                            "/",
                            "/api/info",
                            "/api/client",
                            "/api/local/info",
                            "/vibeface_secure.js",
                            "/vibeface_secure_bg.wasm",
                            "/generated/vibeface_secure.js",
                            "/generated/vibeface_secure_bg.wasm",
                        ] {
                            assert_eq!(
                                client.get(format!("{origin}{path}")).send().await?.status(),
                                StatusCode::NOT_FOUND
                            );
                        }
                    }
                }
                for path in [HOOKS, HOOKS_WAIT, TRIAGE, LOG] {
                    let url = format!("{}{path}", origins[0]);
                    let event = json!({"session_id":"s","turn_id":"t","action":"events"});
                    let status = |request: reqwest::RequestBuilder| async {
                        Ok::<_, anyhow::Error>(request.send().await?.status())
                    };
                    let body = match path {
                        HOOKS => json!({"session_id":"s","turn_id":"t"}),
                        HOOKS_WAIT => json!({}),
                        LOG => json!({"source":"node","records":[]}),
                        _ => event,
                    };
                    assert_eq!(
                        status(client.post(&url).json(&body)).await?,
                        StatusCode::UNAUTHORIZED
                    );
                    assert_eq!(
                        status(client.post(&url).bearer_auth("wrong-owner").json(&body)).await?,
                        StatusCode::UNAUTHORIZED
                    );
                    assert_eq!(
                        status(
                            client
                                .post(&url)
                                .bearer_auth(&launcher.capability)
                                .json(&body)
                        )
                        .await?,
                        StatusCode::BAD_REQUEST
                    );
                    assert_eq!(
                        status(
                            client
                                .post(&url)
                                .header(header::ORIGIN, &origins[0])
                                .json(&body)
                        )
                        .await?,
                        StatusCode::FORBIDDEN
                    );
                    assert_eq!(
                        status(
                            client
                                .post(&url)
                                .header("x-forwarded-for", "203.0.113.1")
                                .json(&body)
                        )
                        .await?,
                        StatusCode::FORBIDDEN
                    );
                    assert_eq!(
                        status(
                            client
                                .post(&url)
                                .bearer_auth(&launcher.capability)
                                .header(header::CONTENT_TYPE, "text/plain")
                                .body(body.to_string())
                        )
                        .await?,
                        StatusCode::UNSUPPORTED_MEDIA_TYPE
                    );
                    assert_eq!(
                        status(client.post(format!("{}{path}", origins[1])).json(&body)).await?,
                        StatusCode::NOT_FOUND
                    );
                }
                assert!(check_hook(&HeaderMap::new(), "192.168.1.2".parse()?).is_err());
                Ok::<_, anyhow::Error>(())
            }
            .await;
            for server in servers {
                server.abort();
            }
            node.shutdown().await;
            drop(node);
            std::fs::remove_dir_all(dir)?;
            result?;
        }
        Ok(())
    }

    #[test]
    fn authorization_requires_one_exact_bearer_header() {
        let mut headers = HeaderMap::new();
        assert!(bearer(&headers).is_none());
        headers.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert_eq!(bearer(&headers), Some("secret"));
        headers.append(header::AUTHORIZATION, "Bearer second".parse().unwrap());
        assert!(bearer(&headers).is_none());
    }
}
