//! Private loopback launch grants. The client still proves its own identity over Noise.
use crate::Entry;
use anyhow::{Result, anyhow, ensure};
use reqwest::{Url, header};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    path::Path,
    time::{Duration, Instant},
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::Request};
use vibeface_bridge::platform::privatefs;
use zeroize::Zeroizing;

const HTTP_LIMIT: usize = 16 * 1024;
const SESSION_SECONDS: u64 = 86_400;

/// Selected Node verified against its current account-private launcher file.
pub struct LocalTarget {
    entry: Entry,
    capability: Zeroizing<String>,
}

/// A local session is not personal-network membership. Its cookie stays in this transport.
#[derive(Clone)]
pub struct LocalGrant {
    entry: Entry,
    network_id: String,
    client_id: String,
    cookie: Zeroizing<String>,
    expires: Instant,
}

impl fmt::Debug for LocalTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalTarget")
            .field("entry", &self.entry)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for LocalGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalGrant")
            .field("entry", &self.entry)
            .field("network_id", &self.network_id)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    origin: String,
    node_id: String,
    capability: Zeroizing<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    url: String,
    device_id: String,
    token: Zeroizing<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Exchange {
    entry: LocalEntry,
    network_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalEntry {
    address: String,
    device_id: String,
    local: bool,
}

fn hex_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn private_origin(origin: &str) -> Result<Url> {
    let url = Url::parse(origin).map_err(|_| anyhow!("invalid private Node origin"))?;
    ensure!(
        url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port().is_some_and(|port| port > 0)
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.origin().ascii_serialization() == origin,
        "invalid private Node origin"
    );
    Ok(url)
}

impl LocalTarget {
    /// Only reads the selected Node's owner-only launcher; never creates or starts a Node.
    pub fn read(data_dir: &Path, expected: &Entry) -> Result<Self> {
        private_origin(&expected.origin)?;
        ensure!(hex_id(&expected.node_id), "invalid selected Node identity");
        let metadata = std::fs::symlink_metadata(data_dir)
            .map_err(|_| anyhow!("local Node directory unavailable"))?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "invalid local Node directory"
        );
        privatefs::ensure_private(data_dir, "the selected local Node directory")
            .map_err(|_| anyhow!("local Node directory must be private"))?;
        let bytes = Zeroizing::new(
            privatefs::read_private_bounded(
                &data_dir.join("local-access.json"),
                4096,
                "the local HTTP credential",
            )
            .map_err(|_| anyhow!("private local HTTP credential unavailable"))?,
        );
        let credential: Credential = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("invalid private local HTTP credential"))?;
        ensure!(
            credential.origin == expected.origin
                && credential.node_id == expected.node_id
                && hex_id(&credential.capability),
            "local HTTP credential does not match the selected Node"
        );
        Ok(Self {
            entry: expected.clone(),
            capability: credential.capability,
        })
    }

    pub async fn grant(&self, client_device_id: &str) -> Result<LocalGrant> {
        ensure!(
            hex_id(client_device_id) && client_device_id != self.entry.node_id,
            "local access requires a separate client identity"
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| anyhow!("local HTTP transport unavailable"))?;
        let mut authorization =
            header::HeaderValue::from_str(&format!("Bearer {}", self.capability.as_str()))
                .map_err(|_| anyhow!("invalid local launcher credential"))?;
        authorization.set_sensitive(true);
        let response = client
            .post(format!("{}/api/local/launch", self.entry.origin))
            .header(header::HOST, self.authority())
            .header(header::ORIGIN, &self.entry.origin)
            .header(header::ACCEPT, "application/json")
            .header(header::AUTHORIZATION, authorization)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|_| anyhow!("local launch request failed"))?;
        let launch: Launch = decode(response, "local launch refused or unavailable").await?;
        ensure!(
            launch.url == self.entry.origin
                && launch.device_id == self.entry.node_id
                && hex_id(&launch.token),
            "local launch does not match the selected Node"
        );
        #[derive(Serialize)]
        struct Body<'a> {
            token: &'a str,
            node_id: &'a str,
            device_id: &'a str,
        }
        let response = client
            .post(format!("{}/api/local/exchange", self.entry.origin))
            .header(header::HOST, self.authority())
            .header(header::ORIGIN, &self.entry.origin)
            .header(header::ACCEPT, "application/json")
            .json(&Body {
                token: &launch.token,
                node_id: &self.entry.node_id,
                device_id: client_device_id,
            })
            .send()
            .await
            .map_err(|_| anyhow!("local session exchange failed"))?;
        ensure!(
            response.status() == reqwest::StatusCode::OK,
            "local launch expired, was used, or was refused"
        );
        let (cookie, lifetime) = session_cookie(response.headers(), &self.entry.node_id)?;
        // Count the response transfer against the cookie lifetime as well.
        let expires = Instant::now() + Duration::from_secs(lifetime);
        let exchange: Exchange = decode(response, "local session exchange refused").await?;
        ensure!(
            exchange.entry.address == self.entry.origin
                && exchange.entry.device_id == self.entry.node_id
                && exchange.entry.local
                && hex_id(&exchange.network_id),
            "local session does not match the selected Node"
        );
        Ok(LocalGrant {
            entry: self.entry.clone(),
            network_id: exchange.network_id,
            client_id: client_device_id.into(),
            cookie,
            expires,
        })
    }

    fn authority(&self) -> &str {
        &self.entry.origin["http://".len()..]
    }
}

impl LocalGrant {
    pub fn entry(&self) -> &Entry {
        &self.entry
    }
    pub fn network_id(&self) -> &str {
        &self.network_id
    }
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Only the connection transport receives the cookie. Noise must still pin `entry.node_id`.
    pub(crate) fn websocket_request(&self, client_id: &str) -> Result<Request<()>> {
        ensure!(
            client_id == self.client_id && Instant::now() < self.expires,
            "local session expired or belongs to another client"
        );
        private_origin(&self.entry.origin)?;
        let url = format!(
            "ws://{}/api/local/ws",
            &self.entry.origin["http://".len()..]
        );
        let mut request = url
            .into_client_request()
            .map_err(|_| anyhow!("invalid local WebSocket request"))?;
        request.headers_mut().insert(
            header::ORIGIN,
            header::HeaderValue::from_str(&self.entry.origin)
                .map_err(|_| anyhow!("invalid local WebSocket origin"))?,
        );
        let mut cookie = header::HeaderValue::from_str(&self.cookie)
            .map_err(|_| anyhow!("invalid local session credential"))?;
        cookie.set_sensitive(true);
        request.headers_mut().insert(header::COOKIE, cookie);
        Ok(request)
    }
}

async fn decode<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    refusal: &'static str,
) -> Result<T> {
    ensure!(response.status() == reqwest::StatusCode::OK, "{refusal}");
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= HTTP_LIMIT as u64),
        "local HTTP response too large"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("local HTTP response interrupted"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= HTTP_LIMIT,
            "local HTTP response too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("invalid local HTTP response"))
}

fn session_cookie(headers: &header::HeaderMap, node_id: &str) -> Result<(Zeroizing<String>, u64)> {
    let name = format!("vibeface_local_{node_id}");
    let mut accepted = None;
    for value in headers.get_all(header::SET_COOKIE).iter() {
        let value = value
            .to_str()
            .map_err(|_| anyhow!("invalid local session cookie"))?;
        let mut parts = value.split(';').map(str::trim);
        let Some((cookie_name, secret)) = parts.next().and_then(|part| part.split_once('=')) else {
            continue;
        };
        if cookie_name != name {
            continue;
        }
        ensure!(
            accepted.is_none() && hex_id(secret),
            "invalid local session cookie"
        );
        let mut http_only = false;
        let mut same_site = false;
        let mut path = false;
        let mut lifetime = None;
        for part in parts {
            if part.eq_ignore_ascii_case("HttpOnly") {
                ensure!(!http_only, "invalid local session cookie");
                http_only = true;
            } else if let Some((key, value)) = part.split_once('=') {
                if key.eq_ignore_ascii_case("SameSite") {
                    ensure!(
                        !same_site && value.eq_ignore_ascii_case("Strict"),
                        "invalid local session cookie"
                    );
                    same_site = true;
                } else if key.eq_ignore_ascii_case("Path") {
                    ensure!(
                        !path && value == "/api/local",
                        "invalid local session cookie"
                    );
                    path = true;
                } else if key.eq_ignore_ascii_case("Max-Age") {
                    ensure!(lifetime.is_none(), "invalid local session cookie");
                    lifetime = value.parse::<u64>().ok();
                    ensure!(lifetime.is_some(), "invalid local session cookie");
                } else {
                    return Err(anyhow!("invalid local session cookie scope"));
                }
            } else {
                return Err(anyhow!("invalid local session cookie attribute"));
            }
        }
        let lifetime = lifetime
            .filter(|seconds| *seconds > 0 && *seconds <= SESSION_SECONDS)
            .ok_or_else(|| anyhow!("local session cookie expired or invalid"))?;
        ensure!(
            http_only && same_site && path,
            "invalid local session cookie protection"
        );
        accepted = Some((Zeroizing::new(format!("{name}={secret}")), lifetime));
    }
    accepted.ok_or_else(|| anyhow!("local session cookie missing"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        io::Write,
        path::PathBuf,
        sync::{Arc, Mutex},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    fn node() -> String {
        "1".repeat(64)
    }
    fn client_id() -> String {
        "2".repeat(64)
    }
    fn network() -> String {
        "3".repeat(64)
    }
    fn capability() -> String {
        "a".repeat(64)
    }
    fn token() -> String {
        "b".repeat(64)
    }
    fn cookie() -> String {
        "c".repeat(64)
    }

    struct Captured {
        path: String,
        headers: BTreeMap<String, String>,
        body: serde_json::Value,
    }
    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }
    impl Reply {
        fn json(body: serde_json::Value) -> Self {
            Self {
                status: 200,
                headers: vec![],
                body: body.to_string(),
            }
        }
    }
    struct Fixture {
        dir: PathBuf,
        entry: Entry,
        requests: Arc<Mutex<Vec<Captured>>>,
        server: JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        async fn new(replies: impl FnOnce(&Entry) -> Vec<Reply>) -> Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let entry = Entry {
                origin: format!("http://{}", listener.local_addr()?),
                node_id: node(),
            };
            let dir =
                std::env::temp_dir().join(format!("vibeface-local-grant-{}", uuid::Uuid::new_v4()));
            privatefs::create_dir_new(&dir)?;
            let mut file = privatefs::create_new(&dir.join("local-access.json"))?;
            file.write_all(serde_json::json!({"origin":entry.origin,"node_id":node(),"capability":capability()}).to_string().as_bytes())?;
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = requests.clone();
            let replies = replies(&entry);
            let server = tokio::spawn(async move {
                for reply in replies {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let (end, length) = loop {
                        let mut chunk = [0; 1024];
                        let read = stream.read(&mut chunk).await.unwrap();
                        assert!(read > 0 && bytes.len() < HTTP_LIMIT);
                        bytes.extend_from_slice(&chunk[..read]);
                        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = std::str::from_utf8(&bytes[..end]).unwrap();
                            let length = head
                                .lines()
                                .filter_map(|line| line.split_once(':'))
                                .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
                            break (end + 4, length);
                        }
                    };
                    while bytes.len() < end + length {
                        let mut chunk = [0; 1024];
                        let read = stream.read(&mut chunk).await.unwrap();
                        assert!(read > 0 && bytes.len() < HTTP_LIMIT);
                        bytes.extend_from_slice(&chunk[..read]);
                    }
                    let head = std::str::from_utf8(&bytes[..end]).unwrap();
                    let mut lines = head.lines();
                    let path = lines.next().unwrap().to_owned();
                    let headers = lines
                        .filter_map(|line| line.split_once(':'))
                        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
                        .collect();
                    let body = serde_json::from_slice(&bytes[end..end + length]).unwrap();
                    captured.lock().unwrap().push(Captured {
                        path,
                        headers,
                        body,
                    });
                    let extra = reply
                        .headers
                        .into_iter()
                        .map(|(name, value)| format!("{name}: {value}\r\n"))
                        .collect::<String>();
                    let response = format!(
                        "HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n{}",
                        reply.status,
                        reply.body.len(),
                        extra,
                        reply.body
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            });
            Ok(Self {
                dir,
                entry,
                requests,
                server,
            })
        }
        fn target(&self) -> Result<LocalTarget> {
            LocalTarget::read(&self.dir, &self.entry)
        }
    }

    fn launch(entry: &Entry) -> Reply {
        Reply::json(serde_json::json!({"url":entry.origin,"device_id":node(),"token":token()}))
    }
    fn exchange(entry: &Entry) -> Reply {
        let mut reply = Reply::json(
            serde_json::json!({"entry":{"address":entry.origin,"device_id":node(),"local":true},"network_id":network()}),
        );
        reply.headers.push((
            "Set-Cookie".into(),
            format!(
                "vibeface_local_{}={}; HttpOnly; SameSite=Strict; Path=/api/local; Max-Age=86400",
                node(),
                cookie()
            ),
        ));
        reply
    }
    fn assert_redacted(value: &str) {
        for secret in [capability(), token(), cookie()] {
            assert!(!value.contains(&secret), "sensitive local grant output");
        }
    }

    #[tokio::test]
    async fn exact_core_exchange_binds_cookie_to_private_ws_and_client() -> Result<()> {
        let fixture = Fixture::new(|entry| vec![launch(entry), exchange(entry)]).await?;
        let target = fixture.target()?;
        assert_redacted(&format!("{target:?}"));
        let grant = target.grant(&client_id()).await?;
        assert_eq!(grant.entry(), &fixture.entry);
        assert_eq!(grant.network_id(), network());
        assert_eq!(grant.client_id(), client_id());
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(
                request.headers["host"],
                fixture.entry.origin.trim_start_matches("http://")
            );
            assert_eq!(request.headers["origin"], fixture.entry.origin);
            assert_eq!(request.headers["content-type"], "application/json");
            assert!(!request.headers.contains_key("cookie"));
        }
        assert_eq!(requests[0].path, "POST /api/local/launch HTTP/1.1");
        assert_eq!(
            requests[0].headers["authorization"],
            format!("Bearer {}", capability())
        );
        assert_eq!(requests[0].body, serde_json::json!({}));
        assert_eq!(requests[1].path, "POST /api/local/exchange HTTP/1.1");
        assert!(!requests[1].headers.contains_key("authorization"));
        assert_eq!(
            requests[1].body,
            serde_json::json!({"token":token(),"node_id":node(),"device_id":client_id()})
        );
        drop(requests);
        let request = grant.websocket_request(&client_id())?;
        assert_eq!(
            request.uri().to_string(),
            format!(
                "ws://{}/api/local/ws",
                fixture.entry.origin.trim_start_matches("http://")
            )
        );
        assert_eq!(request.headers()["origin"], fixture.entry.origin);
        assert_eq!(
            request.headers()["host"],
            fixture.entry.origin.trim_start_matches("http://")
        );
        assert_eq!(
            request.headers()["cookie"],
            format!("vibeface_local_{}={}", node(), cookie())
        );
        assert!(request.headers()["cookie"].is_sensitive());
        assert_redacted(&format!("{grant:?} {request:?}"));
        assert!(grant.websocket_request(&"4".repeat(64)).is_err());
        let mut expired = grant;
        expired.expires = Instant::now() - Duration::from_secs(1);
        assert!(expired.websocket_request(&client_id()).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn private_credential_and_exact_origin_are_required_before_http() -> Result<()> {
        let fixture = Fixture::new(|_| vec![]).await?;
        let mut wrong = fixture.entry.clone();
        wrong.node_id = "4".repeat(64);
        assert!(LocalTarget::read(&fixture.dir, &wrong).is_err());
        for origin in [
            "https://127.0.0.1:1234",
            "http://localhost:1234",
            "http://127.0.0.2:1234",
            "http://[::1]:1234",
            "http://127.0.0.1",
            "http://127.0.0.1:0",
            "http://127.0.0.1:1234/",
            "http://127.0.0.1:1234/api/local",
            "http://127.0.0.1:1234?x=1",
            "http://127.0.0.1:1234#secret",
            "http://user@127.0.0.1:1234",
            "http://2130706433:1234",
            "http://example.invalid:1234",
        ] {
            wrong.origin = origin.into();
            assert!(
                LocalTarget::read(&fixture.dir, &wrong).is_err(),
                "accepted {origin}"
            );
        }
        wrong = fixture.entry.clone();
        wrong.origin = "http://127.0.0.1:1234".into();
        assert!(LocalTarget::read(&fixture.dir, &wrong).is_err());
        assert!(fixture.target()?.grant(&node()).await.is_err());
        std::fs::remove_file(fixture.dir.join("local-access.json"))?;
        assert!(fixture.target().is_err());
        assert!(fixture.requests.lock().unwrap().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn changed_launch_or_exchange_identity_and_origin_are_refused() -> Result<()> {
        for stage in [
            "launch_node",
            "launch_origin",
            "entry_node",
            "entry_origin",
            "entry_local",
            "network",
        ] {
            let fixture = Fixture::new(|entry| {
                let mut first = launch(entry);
                let mut second = exchange(entry);
                let mut body: serde_json::Value =
                    serde_json::from_str(if stage.starts_with("launch") {
                        &first.body
                    } else {
                        &second.body
                    })
                    .unwrap();
                match stage {
                    "launch_node" => body["device_id"] = "4".repeat(64).into(),
                    "launch_origin" => body["url"] = "http://example.invalid:1234".into(),
                    "entry_node" => body["entry"]["device_id"] = "4".repeat(64).into(),
                    "entry_origin" => body["entry"]["address"] = "http://127.0.0.1:1234".into(),
                    "entry_local" => body["entry"]["local"] = false.into(),
                    _ => body["network_id"] = "bad".into(),
                }
                if stage.starts_with("launch") {
                    first.body = body.to_string();
                    vec![first]
                } else {
                    second.body = body.to_string();
                    vec![first, second]
                }
            })
            .await?;
            let error = fixture.target()?.grant(&client_id()).await.unwrap_err();
            assert_redacted(&format!("{error:#?}"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn redirects_replay_and_expired_launch_are_not_retried() -> Result<()> {
        for during_exchange in [false, true] {
            for status in [301, 302, 307, 308] {
                let fixture = Fixture::new(|entry| {
                    let redirect = Reply {
                        status,
                        headers: vec![("Location".into(), "http://example.invalid/secret".into())],
                        body: token(),
                    };
                    if during_exchange {
                        vec![launch(entry), redirect]
                    } else {
                        vec![redirect]
                    }
                })
                .await?;
                let error = fixture.target()?.grant(&client_id()).await.unwrap_err();
                assert_redacted(&format!("{error:#?}"));
                assert_eq!(
                    fixture.requests.lock().unwrap().len(),
                    if during_exchange { 2 } else { 1 }
                );
            }
        }
        let refused = || Reply {
            status: 401,
            headers: vec![],
            body: format!("{} {}", token(), cookie()),
        };
        let fixture =
            Fixture::new(|entry| vec![launch(entry), exchange(entry), launch(entry), refused()])
                .await?;
        let target = fixture.target()?;
        target.grant(&client_id()).await?;
        assert_redacted(&format!(
            "{:#?}",
            target.grant(&client_id()).await.unwrap_err()
        ));
        assert_eq!(fixture.requests.lock().unwrap().len(), 4);
        let expired = Fixture::new(|entry| vec![launch(entry), refused()]).await?;
        assert!(expired.target()?.grant(&client_id()).await.is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shared_or_symlinked_launcher_files_are_refused() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let fixture = Fixture::new(|_| vec![]).await?;
        let path = fixture.dir.join("local-access.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
        assert!(fixture.target().is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let saved = fixture.dir.join("fixture-credential.json");
        std::fs::rename(&path, &saved)?;
        symlink(&saved, &path)?;
        assert!(fixture.target().is_err());
        std::fs::remove_file(&path)?;
        std::fs::rename(&saved, &path)?;
        std::fs::set_permissions(&fixture.dir, std::fs::Permissions::from_mode(0o755))?;
        assert!(fixture.target().is_err());
        std::fs::set_permissions(&fixture.dir, std::fs::Permissions::from_mode(0o700))?;
        let link = fixture.dir.join("linked-directory");
        symlink(&fixture.dir, &link)?;
        assert!(LocalTarget::read(&link, &fixture.entry).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn cookie_scope_protection_expiry_and_response_bounds_are_enforced() -> Result<()> {
        for alteration in [
            "no_http_only",
            "lax",
            "root_path",
            "expired",
            "domain",
            "duplicate",
            "wrong_node",
        ] {
            let fixture = Fixture::new(|entry| {
                let mut reply = exchange(entry);
                let value = &mut reply.headers[0].1;
                match alteration {
                    "no_http_only" => *value = value.replace("; HttpOnly", ""),
                    "lax" => *value = value.replace("Strict", "Lax"),
                    "root_path" => *value = value.replace("Path=/api/local", "Path=/"),
                    "expired" => *value = value.replace("86400", "0"),
                    "domain" => value.push_str("; Domain=127.0.0.1"),
                    "wrong_node" => *value = value.replace(&node(), &"4".repeat(64)),
                    _ => reply.headers.push(reply.headers[0].clone()),
                }
                vec![launch(entry), reply]
            })
            .await?;
            assert_redacted(&format!(
                "{:#?}",
                fixture.target()?.grant(&client_id()).await.unwrap_err()
            ));
        }
        for body in [
            format!("{{\"token\":\"{}\"", token()),
            "x".repeat(HTTP_LIMIT + 1),
        ] {
            let fixture = Fixture::new(|_| {
                vec![Reply {
                    status: 200,
                    headers: vec![],
                    body,
                }]
            })
            .await?;
            assert_redacted(&format!(
                "{:#?}",
                fixture.target()?.grant(&client_id()).await.unwrap_err()
            ));
        }
        Ok(())
    }
}
