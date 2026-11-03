//! `fetch_url`: one public web page read with GET, as text. It reaches only public unicast
//! addresses, so a page cannot steer it into the local network or this Node's own API.
use anyhow::{Context as _, Result, bail, ensure};
use futures_util::StreamExt as _;
use reqwest::Url;
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_TEXT_CHARS: usize = 20_000;
const MAX_REDIRECTS: usize = 5;

/// Whether `ip` is a public unicast address. Everything a private network, this host or a
/// translation prefix could answer on is refused.
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            let [first, second, ..] = ip.segments();
            // Only global unicast 2000::/3, without documentation, Teredo and 6to4, which
            // carry IPv4 addresses inside.
            first & 0xe000 == 0x2000
                && !(first == 0x2001 && second == 0x0db8)
                && !(first == 0x2001 && second == 0)
                && first != 0x2002
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..20).contains(&b)))
}

/// The one public address the request for `url` connects to.
async fn resolve(url: &Url) -> Result<SocketAddr> {
    let host = url.host_str().context("the URL has no host")?;
    let port = url.port_or_known_default().context("the URL has no port")?;
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    let addresses: Vec<SocketAddr> = match literal.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port))
            .await
            .with_context(|| format!("{host} does not resolve"))?
            .collect(),
    };
    ensure!(!addresses.is_empty(), "{host} does not resolve");
    ensure!(
        addresses.iter().all(|address| public(address.ip())),
        "{host} is on a local or private network; fetch_url reads only the public web"
    );
    Ok(addresses[0])
}

fn checked(url: Url) -> Result<Url> {
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "fetch_url reads only http and https"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "fetch_url sends no credentials"
    );
    Ok(url)
}

/// GETs `url` without cookies, credentials or a proxy, following at most 5 redirects, each
/// checked again, and returns the page as text.
pub async fn fetch_url(url: &str) -> Result<Value> {
    tokio::time::timeout(TIMEOUT, fetch(url))
        .await
        .context("the page did not load within 15 seconds")?
}

async fn fetch(url: &str) -> Result<Value> {
    let mut url = checked(Url::parse(url).context("invalid URL")?)?;
    for _ in 0..=MAX_REDIRECTS {
        let address = resolve(&url).await?;
        let mut client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("vibeface/", env!("CARGO_PKG_VERSION")));
        if let Some(host) = url.domain() {
            // Connect to the address checked above, not to a second lookup's answer.
            client = client.resolve(host, address);
        }
        let response = client
            .build()?
            .get(url.clone())
            .header(
                reqwest::header::ACCEPT,
                "text/html,text/plain,application/json;q=0.9,*/*;q=0.1",
            )
            .send()
            .await
            .context("the page could not be fetched")?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .context("a redirect without a location")?;
            url = checked(url.join(location).context("invalid redirect")?)?;
            continue;
        }
        return read(response).await;
    }
    bail!("more than {MAX_REDIRECTS} redirects")
}

async fn read(response: reqwest::Response) -> Result<Value> {
    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let textual = content_type.is_empty()
        || content_type.starts_with("text/")
        || ["json", "xml", "javascript"]
            .iter()
            .any(|kind| content_type.contains(kind));
    ensure!(textual, "the page is {content_type}, not text");
    let mut body = Vec::new();
    let mut truncated = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("the page stopped loading")?;
        let room = MAX_BODY_BYTES - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    let raw = String::from_utf8_lossy(&body);
    let (title, text) = if content_type.contains("html") || looks_html(&raw) {
        (title(&raw), html_text(&raw))
    } else {
        (None, raw.into_owned())
    };
    let kept: String = text.chars().take(MAX_TEXT_CHARS).collect();
    truncated |= kept.len() < text.len();
    Ok(
        json!({"status":status,"final_url":final_url,"content_type":content_type,
        "title":title,"text":kept,"truncated":truncated}),
    )
}

fn looks_html(text: &str) -> bool {
    let start = text
        .trim_start()
        .get(..15)
        .unwrap_or("")
        .to_ascii_lowercase();
    start.starts_with("<!doctype html") || start.starts_with("<html")
}

fn title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let start = start + lower[start..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    Some(collapse(&entities(&html[start..end]))).filter(|title| !title.is_empty())
}

/// The visible text of an HTML page: no scripts, styles or tags, entities decoded, one blank
/// line between blocks. ponytail: a tag scanner, not a parser; malformed pages read roughly.
pub fn html_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::new();
    let mut at = 0;
    while let Some(open) = lower[at..].find('<').map(|offset| at + offset) {
        out.push_str(&entities(&html[at..open]));
        let Some(close) = lower[open..].find('>').map(|offset| open + offset) else {
            at = html.len();
            break;
        };
        let name: String = lower[open + 1..close]
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        at = close + 1;
        if lower[open..].starts_with("<!--") {
            at = lower[open..]
                .find("-->")
                .map_or(html.len(), |end| open + end + 3);
            continue;
        }
        if matches!(
            name.as_str(),
            "script" | "style" | "noscript" | "template" | "svg"
        ) && !lower[open + 1..].starts_with('/')
        {
            let end = format!("</{name}");
            at = lower[at..]
                .find(&end)
                .and_then(|offset| {
                    lower[at + offset..]
                        .find('>')
                        .map(|close| at + offset + close + 1)
                })
                .unwrap_or(html.len());
            continue;
        }
        if matches!(
            name.as_str(),
            "p" | "div"
                | "br"
                | "li"
                | "tr"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "section"
                | "article"
                | "header"
                | "footer"
                | "pre"
                | "table"
                | "ul"
                | "ol"
                | "title"
        ) {
            out.push('\n');
        }
    }
    out.push_str(&entities(&html[at.min(html.len())..]));
    out.lines()
        .map(collapse)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let decoded = rest.find(';').filter(|end| *end <= 10).and_then(|end| {
            let name = &rest[1..end];
            let character = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .map(|hex| u32::from_str_radix(hex, 16))
                    .or_else(|| name.strip_prefix('#').map(str::parse))
                    .and_then(Result::ok)
                    .and_then(char::from_u32),
            };
            character.map(|character| (character, end))
        });
        match decoded {
            Some((character, end)) => {
                out.push(character);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_unicast_addresses_are_reached() {
        for refused in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:8.8.8.8",
            "64:ff9b::a00:1",
            "2001:db8::1",
            "2002:a00:1::1",
            "2001::1",
        ] {
            assert!(
                !public(refused.parse().unwrap()),
                "{refused} must be refused"
            );
        }
        for allowed in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(public(allowed.parse().unwrap()), "{allowed} is public");
        }
    }

    #[tokio::test]
    async fn local_targets_and_other_schemes_are_refused_before_any_request() {
        for url in [
            "http://127.0.0.1:4790/rpc",
            "http://localhost/",
            "http://[::1]/",
            "http://169.254.169.254/latest/meta-data",
            "http://10.0.0.1/",
            "http://2130706433/",
            "http://0x7f.1/",
            "file:///etc/passwd",
            "ftp://example.com/",
            "https://user:secret@example.com/",
        ] {
            assert!(fetch_url(url).await.is_err(), "{url} must be refused");
        }
    }

    #[test]
    fn html_reads_as_its_visible_text() {
        let page = "<!doctype html><html><head><title>A &amp; B</title><style>p{}</style>\
            <script>alert('x<p>')</script></head><body><h1>Head</h1><p>One&nbsp;two &#x41;&#66;</p>\
            <!-- hidden --><div>Last <b>bold</b></div></body></html>";
        assert_eq!(title(page).as_deref(), Some("A & B"));
        assert_eq!(html_text(page), "A & B\nHead\nOne two AB\nLast bold");
    }
}
