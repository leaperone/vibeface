use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Response, StatusCode, Url};
use semver::Version;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, time::Duration};
use vibeface_bridge::transport;
use vibeface_control::{DATA_EPOCH, UPDATE_PROTOCOL};

const GITHUB: &str = "https://api.github.com/repos/leaperone/vibeface";
const RELEASES: &str = "https://github.com/leaperone/vibeface/releases/download";
const MAX_REFS: usize = 12 * 1024 * 1024;
const MAX_METADATA: usize = 1024 * 1024;
const MAX_ASSET: u64 = 160 * 1024 * 1024;
const MAX_CANDIDATES: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Stable,
    Candidate,
    Hourly,
}

impl Channel {
    fn of(version: &Version) -> Option<Self> {
        if !version.build.is_empty() {
            return None;
        }
        let pre = version.pre.as_str();
        if pre.is_empty() {
            return Some(Self::Stable);
        }
        let (label, number) = pre.split_once('.')?;
        if !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        match label {
            "rc" if number.parse::<u64>().is_ok_and(|number| number > 0) => Some(Self::Candidate),
            "hourly" if number.len() == 10 && number[8..].parse::<u8>().ok()? < 24 => {
                Some(Self::Hourly)
            }
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Candidate => "rc",
            Self::Hourly => "hourly",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Candidate {
    pub version: Version,
    pub source_commit: String,
    pub asset: Record,
    pub signed: bool,
    pub current_asset_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Deserialize)]
struct TagRef {
    #[serde(rename = "ref")]
    name: String,
    object: GitObject,
}

#[derive(Clone, Deserialize)]
struct GitObject {
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

#[derive(Deserialize)]
struct AnnotatedTag {
    object: GitObject,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    size: u64,
    digest: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    app: String,
    version: Version,
    tag: String,
    channel: String,
    source_commit: String,
    contracts: Contracts,
    dependencies: BTreeMap<String, Pin>,
    assets: Vec<Record>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Contracts {
    update_protocol: u32,
    data_epoch: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pin {
    tag: String,
    version: Version,
    source_commit: String,
    manifest_sha256: String,
}

#[derive(Deserialize)]
struct Provenance {
    app: String,
    version: Version,
    tag: String,
    channel: String,
    target: String,
    source_commit: String,
    signed: bool,
    signing: String,
    artifacts: BTreeMap<String, String>,
}

fn hex_digest(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn candidate_refs(
    refs: Vec<TagRef>,
    current: &Version,
    channel: Channel,
) -> Vec<(Version, GitObject)> {
    let mut candidates: Vec<_> = refs
        .into_iter()
        .filter_map(|reference| {
            let text = reference.name.strip_prefix("refs/tags/desktop-v")?;
            let version = Version::parse(text).ok()?;
            (version > *current
                && Channel::of(&version) == Some(channel)
                && text == version.to_string())
            .then_some((version, reference.object))
        })
        .collect();
    candidates.sort_unstable_by(|(left, _), (right, _)| right.cmp(left));
    candidates
}

fn trusted_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "api.github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

pub(super) fn client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("vibeface-desktop/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() < 5
                && trusted_url(attempt.url())
                && !attempt
                    .previous()
                    .iter()
                    .any(|url| url.host_str() == Some("api.github.com"))
            {
                attempt.follow()
            } else {
                attempt.error("unapproved Desktop update redirect")
            }
        }))
        .build()
        .context("create Desktop update client")
}

async fn request(client: &Client, url: &str) -> Result<Response> {
    let url = Url::parse(url)?;
    ensure!(trusted_url(&url), "unapproved Desktop update URL");
    client
        .get(url)
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .context("request official Desktop release")
}

async fn body(response: Response, limit: usize) -> Result<Vec<u8>> {
    let response = response
        .error_for_status()
        .map_err(reqwest::Error::without_url)?;
    let mut bytes = Vec::new();
    transport::body(response, limit, &mut bytes)
        .await
        .context("read Desktop update metadata")?;
    Ok(bytes)
}

async fn json<T: DeserializeOwned>(client: &Client, url: &str, limit: usize) -> Result<T> {
    Ok(serde_json::from_slice(
        &body(request(client, url).await?, limit).await?,
    )?)
}

fn target() -> Result<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-gnu"),
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu"),
        ("x86_64", "windows") => Ok("x86_64-pc-windows-msvc"),
        _ => bail!("Desktop updates are unavailable for this target"),
    }
}

fn asset_name(version: &Version, target: &str, suffix: &str) -> String {
    format!("vibeface-desktop-{version}-{target}{suffix}")
}

fn asset_url(version: &Version, name: &str) -> String {
    format!("{RELEASES}/desktop-v{version}/{name}")
}

impl Candidate {
    pub(super) fn validate(&self, current: &Version, suffix: &str) -> Result<()> {
        let channel = Channel::of(current).context("unrecognized Desktop update channel")?;
        let target = target()?;
        ensure!(
            self.version > *current
                && Channel::of(&self.version) == Some(channel)
                && hex_digest(&self.source_commit, 40)
                && self.asset.name == asset_name(&self.version, target, suffix)
                && (1..=MAX_ASSET).contains(&self.asset.size)
                && hex_digest(&self.asset.sha256, 64)
                && match (&self.current_asset_sha256, suffix) {
                    (Some(digest), ".AppImage") => hex_digest(digest, 64),
                    (None, ".AppImage") | (Some(_), _) => false,
                    (None, _) => true,
                }
                && (!target.ends_with("apple-darwin") || self.signed)
                && (!(target.ends_with("windows-msvc") && channel == Channel::Stable)
                    || self.signed),
            "invalid Desktop update identity"
        );
        Ok(())
    }

    pub(super) async fn download(&self, client: &Client) -> Result<Response> {
        request(client, &asset_url(&self.version, &self.asset.name))
            .await?
            .error_for_status()
            .map_err(reqwest::Error::without_url)
            .context("download verified Desktop release asset")
    }
}

async fn commit(client: &Client, mut object: GitObject) -> Result<String> {
    for _ in 0..8 {
        ensure!(
            hex_digest(&object.sha, 40),
            "invalid Desktop release tag SHA"
        );
        match object.kind.as_str() {
            "commit" => return Ok(object.sha),
            "tag" => {
                let annotation: AnnotatedTag = json(
                    client,
                    &format!("{GITHUB}/git/tags/{}", object.sha),
                    MAX_METADATA,
                )
                .await?;
                object = annotation.object;
            }
            _ => bail!("Desktop release tag does not resolve to a commit"),
        }
    }
    bail!("Desktop release tag nesting exceeds limit")
}

fn unique_asset<'a>(release: &'a Release, name: &str) -> Result<Option<&'a GitHubAsset>> {
    let mut matches = release.assets.iter().filter(|asset| asset.name == name);
    let first = matches.next();
    ensure!(matches.next().is_none(), "duplicate Desktop asset {name}");
    Ok(first)
}

fn bound_record(release: &Release, name: &str, record: &Record) -> Result<()> {
    let asset =
        unique_asset(release, name)?.context("Desktop release is missing a published asset")?;
    ensure!(
        record.name == name
            && hex_digest(&record.sha256, 64)
            && asset.size == record.size
            && asset
                .digest
                .as_deref()
                .and_then(|digest| digest.strip_prefix("sha256:"))
                == Some(&record.sha256),
        "Desktop completion manifest differs from GitHub asset metadata"
    );
    Ok(())
}

fn manifest_record(manifest: &Manifest, name: &str) -> Result<Record> {
    let mut matches = manifest.assets.iter().filter(|asset| asset.name == name);
    let record = matches
        .next()
        .context("Desktop completion manifest is missing an asset")?;
    ensure!(
        matches.next().is_none(),
        "duplicate Desktop completion asset {name}"
    );
    Ok(record.clone())
}

fn verify_manifest(
    manifest: &Manifest,
    channel: Channel,
    version: &Version,
    source: &str,
) -> Result<()> {
    let cli = manifest
        .dependencies
        .get("cli")
        .context("Desktop release is missing its CLI pin")?;
    ensure!(
        manifest.schema == 1
            && manifest.app == "desktop"
            && manifest.version == *version
            && manifest.channel == channel.name()
            && manifest.tag == format!("desktop-v{version}")
            && manifest.source_commit == source
            && manifest.contracts.update_protocol == UPDATE_PROTOCOL
            && manifest.contracts.data_epoch == DATA_EPOCH
            && manifest.dependencies.len() == 1
            && Channel::of(&cli.version).is_some()
            && cli.tag == format!("cli-v{}", cli.version)
            && hex_digest(&cli.source_commit, 40)
            && hex_digest(&cli.manifest_sha256, 64),
        "Desktop completion manifest identity mismatch"
    );
    Ok(())
}

async fn metadata(client: &Client, version: &Version, record: &Record) -> Result<Vec<u8>> {
    ensure!(
        (1..=MAX_METADATA as u64).contains(&record.size),
        "Desktop metadata exceeds limit"
    );
    let bytes = body(
        request(client, &asset_url(version, &record.name)).await?,
        record.size as usize,
    )
    .await?;
    ensure!(
        bytes.len() as u64 == record.size && hex::encode(Sha256::digest(&bytes)) == record.sha256,
        "Desktop metadata digest or size mismatch"
    );
    Ok(bytes)
}

async fn completion_manifest(
    client: &Client,
    release: &Release,
    version: &Version,
) -> Result<Option<Manifest>> {
    let Some(asset) = unique_asset(release, "release.json")? else {
        return Ok(None);
    };
    let record = Record {
        name: asset.name.clone(),
        size: asset.size,
        sha256: asset
            .digest
            .as_deref()
            .and_then(|digest| digest.strip_prefix("sha256:"))
            .filter(|digest| hex_digest(digest, 64))
            .context("missing Desktop completion digest")?
            .to_owned(),
    };
    Ok(Some(serde_json::from_slice(
        &metadata(client, version, &record).await?,
    )?))
}

async fn current_appimage_digest(
    client: &Client,
    version: &Version,
    channel: Channel,
    target: &str,
) -> Result<String> {
    let tag = format!("desktop-v{version}");
    let release: Release = json(
        client,
        &format!("{GITHUB}/releases/tags/{tag}"),
        MAX_METADATA,
    )
    .await?;
    ensure!(
        release.tag_name == tag
            && !release.draft
            && release.prerelease == (channel != Channel::Stable),
        "current AppImage release identity mismatch"
    );
    let manifest = completion_manifest(client, &release, version)
        .await?
        .context("current AppImage has no completed release")?;
    let reference: TagRef = json(
        client,
        &format!("{GITHUB}/git/ref/tags/{tag}"),
        MAX_METADATA,
    )
    .await?;
    ensure!(
        reference.name == format!("refs/tags/{tag}"),
        "current AppImage tag identity mismatch"
    );
    verify_manifest(
        &manifest,
        channel,
        version,
        &commit(client, reference.object).await?,
    )?;
    let name = asset_name(version, target, ".AppImage");
    let asset = manifest_record(&manifest, &name)?;
    bound_record(&release, &name, &asset)?;
    Ok(asset.sha256)
}

pub(super) async fn check(
    client: &Client,
    current: &Version,
    installed_source: &str,
    suffix: &str,
) -> Result<Option<Candidate>> {
    let channel = Channel::of(current).context("unrecognized Desktop update channel")?;
    let refs: Vec<TagRef> = json(
        client,
        &format!("{GITHUB}/git/matching-refs/tags/desktop-v"),
        MAX_REFS,
    )
    .await?;
    let target = target()?;
    let installed_tag = format!("refs/tags/desktop-v{current}");
    let installed_object = refs
        .iter()
        .find(|reference| reference.name == installed_tag)
        .context("current Desktop build has no official tag")?
        .object
        .clone();
    ensure!(
        commit(client, installed_object).await? == installed_source,
        "current Desktop build differs from the official tag"
    );
    let candidates = candidate_refs(refs, current, channel);
    let exceeds_limit = candidates.len() > MAX_CANDIDATES;
    for (version, object) in candidates.into_iter().take(MAX_CANDIDATES) {
        let tag = format!("desktop-v{version}");
        let response = request(client, &format!("{GITHUB}/releases/tags/{tag}")).await?;
        if response.status() == StatusCode::NOT_FOUND {
            continue;
        }
        let release: Release = serde_json::from_slice(&body(response, MAX_METADATA).await?)?;
        ensure!(
            release.tag_name == tag,
            "GitHub Desktop release tag mismatch"
        );
        if release.draft || release.prerelease != (channel != Channel::Stable) {
            continue;
        }
        let Some(manifest) = completion_manifest(client, &release, &version).await? else {
            continue;
        };
        let source_commit = commit(client, object).await?;
        verify_manifest(&manifest, channel, &version, &source_commit)?;
        let name = asset_name(&version, target, suffix);
        let asset = manifest_record(&manifest, &name)?;
        bound_record(&release, &name, &asset)?;
        let name = asset_name(&version, target, ".provenance.json");
        let receipt = manifest_record(&manifest, &name)?;
        bound_record(&release, &name, &receipt)?;
        let provenance: Provenance =
            serde_json::from_slice(&metadata(client, &version, &receipt).await?)?;
        let signing = match (target, provenance.signed) {
            ("aarch64-apple-darwin", true) => "developer-id-notarized",
            ("x86_64-pc-windows-msvc", true) => "authenticode",
            ("x86_64-pc-windows-msvc", false) => "unsigned-development-preview",
            ("x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu", false) => {
                "platform-checksums"
            }
            _ => bail!("unexpected Desktop signing state"),
        };
        ensure!(
            provenance.app == "desktop"
                && provenance.tag == tag
                && provenance.version == version
                && provenance.target == target
                && provenance.source_commit == source_commit
                && provenance.channel == channel.name()
                && provenance.signing == signing
                && provenance.artifacts.get(&asset.name) == Some(&asset.sha256),
            "Desktop asset provenance does not match the release"
        );
        let current_asset_sha256 = if suffix == ".AppImage" {
            Some(current_appimage_digest(client, current, channel, target).await?)
        } else {
            None
        };
        let candidate = Candidate {
            version,
            source_commit,
            asset,
            signed: provenance.signed,
            current_asset_sha256,
        };
        candidate.validate(current, suffix)?;
        return Ok(Some(candidate));
    }
    ensure!(
        !exceeds_limit,
        "cannot confirm the latest complete Desktop release within the lookup limit"
    );
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_only_newer_desktop_tags_in_the_same_channel() {
        let refs = [
            "desktop-v0.0.1-hourly.2026111705",
            "desktop-v0.0.1-hourly.2026111817",
            "desktop-v0.0.2-rc.3",
            "desktop-v0.0.2",
            "desktop-v0.0.1-hourly.2026111807",
            "cli-v0.0.1-hourly.2026111820",
            "desktop-v0.0.1-hourly.2026111825",
        ]
        .into_iter()
        .map(|tag| TagRef {
            name: format!("refs/tags/{tag}"),
            object: GitObject {
                kind: "commit".into(),
                sha: "a".repeat(40),
            },
        })
        .collect();
        let result = candidate_refs(
            refs,
            &"0.0.1-hourly.2026111706".parse().unwrap(),
            Channel::Hourly,
        );
        assert_eq!(
            result
                .into_iter()
                .map(|(version, _)| version.to_string())
                .collect::<Vec<_>>(),
            ["0.0.1-hourly.2026111817", "0.0.1-hourly.2026111807"]
        );
    }

    #[test]
    fn redirect_hosts_are_pinned_to_github_https() {
        assert!(trusted_url(
            &Url::parse("https://release-assets.githubusercontent.com/download").unwrap()
        ));
        for url in [
            "http://github.com/",
            "https://github.com.invalid/",
            "https://user@github.com/",
            "https://github.com:1443/",
        ] {
            assert!(!trusted_url(&Url::parse(url).unwrap()));
        }
    }
}
