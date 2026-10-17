use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use flate2::bufread::GzDecoder;
use reqwest::{Client, Url};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const API: &str = "https://api.github.com/repos/leaperone/vibeface";
const MAX_PAGES: usize = 20;
const MAX_JSON: u64 = 4 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 128 * 1024 * 1024;
const MAX_UNPACKED: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;

#[derive(ValueEnum, Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Stable,
    Nightly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub target: String,
    pub source_commit: String,
    pub archive_url: String,
    pub checksum_url: String,
    pub archive_size: u64,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Deserialize)]
struct GitObject {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct ObjectResponse {
    object: GitObject,
}

/// Find a newer official release in the selected channel without running providers.
pub async fn check(channel: Channel) -> Result<Option<Release>> {
    let client = client()?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let mut selected: Option<GithubRelease> = None;
    let mut selected_version = current;
    for page in 1..=MAX_PAGES {
        let releases: Vec<GithubRelease> =
            get_json(&client, &format!("{API}/releases?per_page=100&page={page}")).await?;
        let complete = releases.len() < 100;
        for release in releases {
            let Some(version) = channel_version(&release.tag_name, channel) else {
                continue;
            };
            if release.draft
                || release.prerelease != (channel == Channel::Nightly)
                || version <= selected_version
            {
                continue;
            }
            selected_version = version;
            selected = Some(release);
        }
        if complete {
            return match selected {
                Some(release) => resolve_release(&client, release, selected_version)
                    .await
                    .map(Some),
                None => Ok(None),
            };
        }
    }
    bail!("release listing reached {MAX_PAGES} pages; cannot confirm a complete listing")
}

fn channel_version(tag: &str, channel: Channel) -> Option<Version> {
    let version = Version::parse(tag.strip_prefix('v')?).ok()?;
    if !version.build.is_empty() || format!("v{version}") != tag {
        return None;
    }
    match channel {
        Channel::Stable if version.pre.is_empty() => Some(version),
        Channel::Nightly => {
            let parts: Vec<_> = version.pre.as_str().split('.').collect();
            if parts.len() != 3
                || parts[0] != "nightly"
                || !valid_date(parts[1])
                || parts[2].parse::<u64>().ok()? == 0
            {
                return None;
            }
            Some(version)
        }
        _ => None,
    }
}

fn valid_date(date: &str) -> bool {
    if date.len() != 8 || !date.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let year = date[..4].parse::<u32>().unwrap_or(0);
    let month = date[4..6].parse::<usize>().unwrap_or(0);
    let day = date[6..].parse::<u32>().unwrap_or(0);
    if year == 0 || !(1..=12).contains(&month) {
        return false;
    }
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    (1..=days[month - 1]).contains(&day)
}

fn host_target() -> Result<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        ("x86_64", "macos") => Ok("x86_64-apple-darwin"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-gnu"),
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu"),
        _ => bail!("updates are not available for this platform"),
    }
}

async fn resolve_release(
    client: &Client,
    release: GithubRelease,
    version: Version,
) -> Result<Release> {
    let target = host_target()?;
    let name = format!("vibeface-node-{version}-{target}.tar.gz");
    let asset = unique_asset(&release.assets, &name)?;
    let checksum = unique_asset(&release.assets, &format!("{name}.sha256"))?;
    ensure!(
        (1..=MAX_ARCHIVE).contains(&asset.size),
        "release archive size exceeds limit"
    );
    ensure!(
        (1..=4096).contains(&checksum.size),
        "release checksum size exceeds limit"
    );
    ensure!(
        asset.browser_download_url == asset_url(&release.tag_name, &name),
        "unexpected release archive URL"
    );
    ensure!(
        checksum.browser_download_url == asset_url(&release.tag_name, &format!("{name}.sha256")),
        "unexpected release checksum URL"
    );
    let source_commit = tag_commit(client, &release.tag_name).await?;
    Ok(Release {
        version,
        tag: release.tag_name,
        target: target.to_owned(),
        source_commit,
        archive_url: asset.browser_download_url.clone(),
        checksum_url: checksum.browser_download_url.clone(),
        archive_size: asset.size,
    })
}

fn unique_asset<'a>(assets: &'a [Asset], name: &str) -> Result<&'a Asset> {
    let mut matches = assets.iter().filter(|asset| asset.name == name);
    let asset = matches
        .next()
        .with_context(|| format!("release is missing {name}"))?;
    ensure!(
        matches.next().is_none(),
        "release contains duplicate asset {name}"
    );
    Ok(asset)
}

fn asset_url(tag: &str, name: &str) -> String {
    format!("https://github.com/leaperone/vibeface/releases/download/{tag}/{name}")
}

fn valid_commit(commit: &str) -> bool {
    commit.len() == 40
        && commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

async fn tag_commit(client: &Client, tag: &str) -> Result<String> {
    let response: ObjectResponse = get_json(client, &format!("{API}/git/ref/tags/{tag}")).await?;
    let mut object = response.object;
    for _ in 0..8 {
        ensure!(valid_commit(&object.sha), "invalid release tag object SHA");
        match object.kind.as_str() {
            "commit" => return Ok(object.sha),
            "tag" => {
                let response: ObjectResponse =
                    get_json(client, &format!("{API}/git/tags/{}", object.sha)).await?;
                object = response.object;
            }
            _ => bail!("release tag does not point to a commit"),
        }
    }
    bail!("release tag nesting exceeds limit")
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
                "api.github.com"
                    | "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

fn client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("vibeface-node/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt
                .previous()
                .iter()
                .any(|url| url.host_str() == Some("api.github.com"))
            {
                attempt.error("GitHub API redirects are not permitted")
            } else if attempt.previous().len() >= 5 || !trusted_url(attempt.url()) {
                attempt.error("update redirect is not an approved HTTPS GitHub URL")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .context("create release download client")
}

async fn get_json<T: serde::de::DeserializeOwned>(client: &Client, url: &str) -> Result<T> {
    serde_json::from_slice(&get_bytes(client, url, MAX_JSON).await?)
        .context("decode GitHub release response")
}

async fn response(client: &Client, url: &str) -> Result<reqwest::Response> {
    let url = Url::parse(url)?;
    ensure!(trusted_url(&url), "update URL is not approved");
    let token = std::env::var("GITHUB_TOKEN").ok();
    request(client, url, token.as_deref())
        .send()
        .await
        .map_err(reqwest::Error::without_url)?
        .error_for_status()
        .map_err(reqwest::Error::without_url)
        .context("fetch official release")
}

fn request(client: &Client, url: Url, token: Option<&str>) -> reqwest::RequestBuilder {
    let api = url.host_str() == Some("api.github.com");
    let request = client.get(url);
    match token.filter(|token| api && !token.is_empty()) {
        Some(token) => request.bearer_auth(token),
        None => request,
    }
}

async fn get_bytes(client: &Client, url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut response = response(client, url).await?;
    ensure!(
        response.content_length().is_none_or(|size| size <= limit),
        "release response exceeds limit"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len() as u64 + chunk.len() as u64 <= limit,
            "release response exceeds limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Download and verify the complete package, then atomically publish its directory.
pub async fn stage(release: &Release, install_root: &Path) -> Result<PathBuf> {
    validate_release(release)?;
    let client = client()?;
    ensure!(
        tag_commit(&client, &release.tag).await? == release.source_commit,
        "release tag changed since update check"
    );
    let name = format!("vibeface-node-{}-{}", release.version, release.target);
    let archive_name = format!("{name}.tar.gz");
    let checksum = get_bytes(&client, &release.checksum_url, 4096).await?;
    let digest = checksum_digest(&checksum, &archive_name)?;
    let releases = install_root.join("releases");
    fs::create_dir_all(&releases)?;
    ensure!(
        fs::symlink_metadata(&releases)?.file_type().is_dir(),
        "release directory is not a regular directory"
    );
    let scratch = Scratch::create(&releases)?;
    let archive_path = scratch.0.join("archive.tar.gz");
    download_archive(&client, release, &archive_path, &digest).await?;
    let unpack_root = scratch.0.clone();
    let expected_name = name.clone();
    let package =
        tokio::task::spawn_blocking(move || unpack(&archive_path, &unpack_root, &expected_name))
            .await??;
    verify_build(&package, release)?;
    verify_binary(&package.join("vibeface-node"), &release.version).await?;
    let destination = releases.join(release.version.to_string());
    match fs::symlink_metadata(&destination) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "installed release path is not a regular directory"
            );
            ensure!(
                tree(&destination)? == tree(&package)?,
                "existing release directory differs from the verified package"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::rename(&package, &destination).context("publish verified release")?;
            File::open(&releases)?.sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(destination)
}

fn validate_release(release: &Release) -> Result<()> {
    let channel = if release.version.pre.is_empty() {
        Channel::Stable
    } else {
        Channel::Nightly
    };
    ensure!(
        channel_version(&release.tag, channel).as_ref() == Some(&release.version),
        "invalid release tag or version"
    );
    ensure!(
        release.target == host_target()?,
        "release target does not match this machine"
    );
    ensure!(
        valid_commit(&release.source_commit),
        "invalid release source commit"
    );
    ensure!(
        (1..=MAX_ARCHIVE).contains(&release.archive_size),
        "release archive size exceeds limit"
    );
    let name = format!(
        "vibeface-node-{}-{}.tar.gz",
        release.version, release.target
    );
    ensure!(
        release.archive_url == asset_url(&release.tag, &name),
        "unexpected release archive URL"
    );
    ensure!(
        release.checksum_url == asset_url(&release.tag, &format!("{name}.sha256")),
        "unexpected release checksum URL"
    );
    Ok(())
}

fn checksum_digest(body: &[u8], expected_name: &str) -> Result<Vec<u8>> {
    let body = std::str::from_utf8(body).context("checksum file is not UTF-8")?;
    let mut lines = body.lines();
    let mut fields = lines.next().unwrap_or_default().split_whitespace();
    let digest = fields.next().unwrap_or_default();
    let filename = fields.next().unwrap_or_default();
    let filename = filename.strip_prefix('*').unwrap_or(filename);
    ensure!(
        fields.next().is_none() && lines.next().is_none(),
        "checksum must contain exactly one entry"
    );
    ensure!(
        filename == expected_name,
        "checksum filename does not match release archive"
    );
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid SHA256 digest"
    );
    hex::decode(digest).context("decode archive SHA256")
}

async fn download_archive(
    client: &Client,
    release: &Release,
    path: &Path,
    expected: &[u8],
) -> Result<()> {
    let mut response = response(client, &release.archive_url).await?;
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size == release.archive_size),
        "release archive length changed"
    );
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .await?;
    let mut size = 0_u64;
    let mut digest = Sha256::new();
    while let Some(chunk) = response.chunk().await? {
        size += chunk.len() as u64;
        ensure!(
            size <= release.archive_size && size <= MAX_ARCHIVE,
            "release archive exceeds declared size"
        );
        digest.update(&chunk);
        file.write_all(&chunk).await?;
    }
    ensure!(
        size == release.archive_size,
        "release archive was truncated"
    );
    ensure!(
        digest.finalize().as_slice() == expected,
        "release archive SHA256 mismatch"
    );
    file.sync_all().await?;
    Ok(())
}

struct Scratch(PathBuf);

impl Scratch {
    fn create(parent: &Path) -> Result<Self> {
        let path = parent.join(format!(".staging-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn unpack(archive: &Path, destination: &Path, root_name: &str) -> Result<PathBuf> {
    let reader = GzDecoder::new(BufReader::new(File::open(archive)?)).take(MAX_UNPACKED + 1);
    let mut archive = tar::Archive::new(reader);
    let mut seen = HashSet::new();
    let mut total = 0_u64;
    for item in archive.entries()? {
        let mut entry = item.context("read package archive entry")?;
        ensure!(seen.len() < MAX_ENTRIES, "package has too many entries");
        let bytes = entry.path_bytes();
        let raw = std::str::from_utf8(&bytes).context("package path is not UTF-8")?;
        ensure!(
            !raw.contains('\\') && !raw.contains('\0'),
            "invalid package path"
        );
        ensure!(
            raw.trim_end_matches('/')
                .split('/')
                .all(|part| !matches!(part, "" | "." | "..")),
            "noncanonical package path"
        );
        let path = Path::new(raw);
        ensure!(
            path.components()
                .all(|part| matches!(part, Component::Normal(_))),
            "unsafe package path"
        );
        ensure!(
            path.components()
                .next()
                .is_some_and(|part| part.as_os_str() == root_name),
            "unexpected package root"
        );
        let relative = path.to_path_buf();
        ensure!(seen.insert(relative.clone()), "duplicate package path");
        let kind = entry.header().entry_type();
        ensure!(
            kind == tar::EntryType::Regular || kind == tar::EntryType::Directory,
            "package contains a link or unsupported entry"
        );
        ensure!(
            entry.header().link_name()?.is_none(),
            "package entry contains a link target"
        );
        let size = entry.header().size()?;
        total = total.checked_add(size).context("package size overflow")?;
        ensure!(total <= MAX_UNPACKED, "unpacked package exceeds limit");
        let output = destination.join(&relative);
        if kind == tar::EntryType::Directory {
            ensure!(size == 0, "package directory has content");
            fs::create_dir_all(&output)?;
            fs::set_permissions(&output, fs::Permissions::from_mode(0o755))?;
        } else {
            ensure!(
                relative.components().count() > 1,
                "package root must be a directory"
            );
            fs::create_dir_all(output.parent().context("package entry has no parent")?)?;
            let mode = if entry.header().mode()? & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&output)?;
            ensure!(
                std::io::copy(&mut entry, &mut file)? == size,
                "truncated package file"
            );
            file.sync_all()?;
        }
    }
    // Drain gzip to check its trailer and reject compressed trailing tar data.
    let mut reader = archive.into_inner();
    let mut tail = [0_u8; 8192];
    loop {
        let count = reader.read(&mut tail)?;
        if count == 0 {
            break;
        }
        ensure!(
            tail[..count].iter().all(|byte| *byte == 0),
            "invalid archive trailer"
        );
    }
    ensure!(reader.limit() > 0, "unpacked archive exceeds limit");
    ensure!(
        reader.into_inner().into_inner().fill_buf()?.is_empty(),
        "archive contains trailing compressed data"
    );
    let root = destination.join(root_name);
    ensure!(root.is_dir(), "package root is missing");
    Ok(root)
}

#[derive(Deserialize)]
struct BuildInfo {
    version: String,
    target: String,
    source_commit: String,
    update_protocol: u32,
    data_epoch: u32,
}

fn verify_build(package: &Path, release: &Release) -> Result<()> {
    ensure!(
        fs::symlink_metadata(package.join("sidecars/node_modules"))
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "release package must not bundle generated SDK dependencies"
    );
    let metadata = fs::metadata(package.join("BUILD-INFO.json"))?;
    ensure!(metadata.len() <= 4096, "BUILD-INFO exceeds limit");
    let build: BuildInfo = serde_json::from_slice(&fs::read(package.join("BUILD-INFO.json"))?)
        .context("package does not declare a supported update protocol and data epoch")?;
    ensure!(
        build.version == release.version.to_string()
            && build.target == release.target
            && build.source_commit == release.source_commit,
        "package build identity does not match the official release tag"
    );
    ensure!(
        build.update_protocol == 1 && build.data_epoch == 1,
        "package requires an unsupported update protocol or data epoch"
    );
    for file in [
        "vibeface-node",
        "README.md",
        "RUNTIME-REQUIREMENTS.md",
        "sidecars/package.json",
        "sidecars/package-lock.json",
        "sidecars/codex-manager.mjs",
        "scripts/install-node-service.sh",
        "docs/node.md",
        "docs/releasing.md",
    ] {
        ensure!(
            fs::symlink_metadata(package.join(file))?
                .file_type()
                .is_file(),
            "package is missing required file {file}"
        );
    }
    ensure!(
        fs::metadata(package.join("vibeface-node"))?
            .permissions()
            .mode()
            & 0o111
            != 0,
        "package binary is not executable"
    );
    Ok(())
}

async fn verify_binary(binary: &Path, version: &Version) -> Result<()> {
    let mut child = tokio::process::Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("run packaged binary version check")?;
    let output = tokio::time::timeout(Duration::from_secs(10), async {
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .context("packaged binary has no stdout")?
            .take(1025)
            .read_to_end(&mut output)
            .await?;
        ensure!(
            output.len() <= 1024,
            "packaged binary version output exceeds limit"
        );
        ensure!(
            child.wait().await?.success(),
            "packaged binary version check failed"
        );
        Ok::<_, anyhow::Error>(output)
    })
    .await
    .context("packaged binary version check timed out")??;
    ensure!(
        std::str::from_utf8(&output)?.trim_end() == format!("vibeface-node {version}"),
        "packaged binary version does not match release"
    );
    Ok(())
}

#[derive(PartialEq, Eq)]
enum TreeEntry {
    Directory,
    File {
        size: u64,
        executable: bool,
        sha256: Vec<u8>,
    },
}

fn tree(root: &Path) -> Result<BTreeMap<PathBuf, TreeEntry>> {
    let mut result = BTreeMap::new();
    let mut pending = vec![PathBuf::new()];
    let mut size = 0_u64;
    while let Some(relative) = pending.pop() {
        for entry in fs::read_dir(root.join(&relative))? {
            let entry = entry?;
            let relative = relative.join(entry.file_name());
            ensure!(
                result.len() < MAX_ENTRIES,
                "existing release contains too many entries"
            );
            let metadata = entry.path().symlink_metadata()?;
            if relative == Path::new("sidecars/node_modules") {
                ensure!(
                    metadata.file_type().is_dir(),
                    "generated SDK dependency path must be a regular directory"
                );
                continue;
            }
            let kind = if metadata.file_type().is_dir() {
                pending.push(relative.clone());
                TreeEntry::Directory
            } else {
                ensure!(
                    metadata.file_type().is_file(),
                    "existing release contains a link or unsupported file"
                );
                size = size
                    .checked_add(metadata.len())
                    .context("existing release size overflow")?;
                ensure!(size <= MAX_UNPACKED, "existing release exceeds size limit");
                let mut file = File::open(entry.path())?;
                let mut digest = Sha256::new();
                std::io::copy(&mut file, &mut digest)?;
                TreeEntry::File {
                    size: metadata.len(),
                    executable: metadata.permissions().mode() & 0o111 != 0,
                    sha256: digest.finalize().to_vec(),
                }
            };
            result.insert(relative, kind);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::os::unix::fs::symlink;

    #[test]
    fn channels_accept_only_canonical_release_tags() {
        assert_eq!(
            channel_version("v1.2.3", Channel::Stable),
            Some(Version::new(1, 2, 3))
        );
        assert!(channel_version("v1.2.3-nightly.20260228.1", Channel::Nightly).is_some());
        assert!(channel_version("v1.2.3-nightly.20240229.10", Channel::Nightly).is_some());
        for tag in [
            "1.2.3",
            "v01.2.3",
            "v1.2.3+build",
            "v1.2.3-rc.1",
            "v1.2.3-nightly.20260229.1",
            "v1.2.3-nightly.20261301.1",
            "v1.2.3-nightly.20260100.1",
            "v1.2.3-nightly.20260101.0",
            "v1.2.3-nightly.20260101.01",
            "v1.2.3-nightly.20260101.1.extra",
        ] {
            assert!(channel_version(tag, Channel::Stable).is_none(), "{tag}");
            assert!(channel_version(tag, Channel::Nightly).is_none(), "{tag}");
        }
        assert!(channel_version("v1.2.3", Channel::Nightly).is_none());
        assert!(channel_version("v1.2.3-nightly.20260101.1", Channel::Stable).is_none());
        assert!(
            Version::parse("1.2.3-nightly.20260102.1").unwrap()
                > Version::parse("1.2.3-nightly.20260101.10").unwrap()
        );
        assert!(
            Version::parse("1.2.3").unwrap() > Version::parse("1.2.3-nightly.20260102.1").unwrap()
        );
    }

    #[test]
    fn download_urls_and_checksums_are_bound_to_the_archive() {
        for good in [
            "https://api.github.com/repos/leaperone/vibeface/releases",
            "https://github.com/leaperone/vibeface/releases/download/tag/package",
            "https://release-assets.githubusercontent.com/path?signature=example",
        ] {
            assert!(trusted_url(&Url::parse(good).unwrap()));
        }
        for bad in [
            "http://github.com/path",
            "https://github.com.evil.example/path",
            "https://user@github.com/path",
            "https://github.com:8443/path",
            "https://github.com/path#fragment",
            "https://127.0.0.1/path",
        ] {
            assert!(!trusted_url(&Url::parse(bad).unwrap()));
        }
        let digest = "a".repeat(64);
        assert_eq!(
            checksum_digest(
                format!("{digest}  package.tar.gz\n").as_bytes(),
                "package.tar.gz"
            )
            .unwrap(),
            vec![0xaa; 32]
        );
        assert!(
            checksum_digest(
                format!("{digest} *package.tar.gz\n").as_bytes(),
                "package.tar.gz"
            )
            .is_ok()
        );
        for bad in [
            format!("{digest} ../package.tar.gz\n"),
            format!("{digest} **package.tar.gz\n"),
            format!("{digest} package.tar.gz\n{digest} package.tar.gz\n"),
            "bad package.tar.gz".to_owned(),
        ] {
            assert!(checksum_digest(bad.as_bytes(), "package.tar.gz").is_err());
        }
    }

    #[test]
    fn optional_github_token_is_only_attached_to_the_api() {
        let client = client().unwrap();
        let api = request(
            &client,
            Url::parse("https://api.github.com/repos/leaperone/vibeface/releases").unwrap(),
            Some("test-token"),
        )
        .build()
        .unwrap();
        assert_eq!(
            api.headers()[reqwest::header::AUTHORIZATION],
            "Bearer test-token"
        );
        assert!(api.headers()[reqwest::header::AUTHORIZATION].is_sensitive());
        for url in [
            "https://github.com/leaperone/vibeface/releases/download/tag/file",
            "https://release-assets.githubusercontent.com/path",
        ] {
            assert!(
                !request(&client, Url::parse(url).unwrap(), Some("test-token"))
                    .build()
                    .unwrap()
                    .headers()
                    .contains_key(reqwest::header::AUTHORIZATION)
            );
        }
        assert!(
            !request(
                &client,
                Url::parse("https://api.github.com/repos/leaperone/vibeface/releases").unwrap(),
                None
            )
            .build()
            .unwrap()
            .headers()
            .contains_key(reqwest::header::AUTHORIZATION)
        );
    }

    fn archive(path: &Path, entries: &[(&str, u8, &[u8])]) {
        let mut builder = tar::Builder::new(GzEncoder::new(
            File::create(path).unwrap(),
            Compression::fast(),
        ));
        for (name, kind, content) in entries {
            let mut header = tar::Header::new_gnu();
            // Raw header names let the test exercise paths rejected by tar::Builder::append_path.
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_entry_type(tar::EntryType::new(*kind));
            header.set_mode(0o755);
            header.set_size(content.len() as u64);
            if *kind == b'2' {
                header.set_link_name("/tmp/outside").unwrap();
            }
            header.set_cksum();
            builder.append(&header, *content).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn unpack_rejects_links_duplicate_paths_and_escaping_roots() {
        let scratch = Scratch::create(&std::env::temp_dir()).unwrap();
        let package = scratch.0.join("package.tar.gz");
        archive(&package, &[("package/file", b'0', b"complete")]);
        let extracted = unpack(&package, &scratch.0.join("good"), "package").unwrap();
        assert_eq!(fs::read(extracted.join("file")).unwrap(), b"complete");
        for (index, entries) in [
            vec![("../outside", b'0', b"x".as_slice())],
            vec![("package/../outside", b'0', b"x".as_slice())],
            vec![("/package/outside", b'0', b"x".as_slice())],
            vec![("other/file", b'0', b"x".as_slice())],
            vec![("package/./file", b'0', b"x".as_slice())],
            vec![("package/file", b'2', b"".as_slice())],
            vec![("package/file", b'1', b"".as_slice())],
            vec![
                ("package/file", b'0', b"x".as_slice()),
                ("package/file", b'0', b"y".as_slice()),
            ],
        ]
        .into_iter()
        .enumerate()
        {
            archive(&package, &entries);
            assert!(unpack(&package, &scratch.0.join(format!("bad-{index}")), "package").is_err());
        }
        assert!(!scratch.0.join("outside").exists());
    }

    #[test]
    fn unpack_rejects_compressed_trailing_data() {
        let scratch = Scratch::create(&std::env::temp_dir()).unwrap();
        let package = scratch.0.join("package.tar.gz");
        archive(&package, &[("package/file", b'0', b"complete")]);
        let mut bytes = fs::read(&package).unwrap();
        bytes.extend_from_slice(b"untrusted trailing data");
        fs::write(&package, bytes).unwrap();
        assert!(unpack(&package, &scratch.0.join("unpack"), "package").is_err());
    }

    #[tokio::test]
    async fn complete_package_identity_and_reuse_are_verified() {
        let scratch = Scratch::create(&std::env::temp_dir()).unwrap();
        let package = scratch.0.join("complete");
        let copy = scratch.0.join("copy");
        fs::create_dir(&package).unwrap();
        fs::create_dir(&copy).unwrap();
        let release = Release {
            version: Version::new(0, 0, 2),
            tag: "v0.0.2".to_owned(),
            target: host_target().unwrap().to_owned(),
            source_commit: "a".repeat(40),
            archive_url: String::new(),
            checksum_url: String::new(),
            archive_size: 1,
        };
        let build = serde_json::json!({"version": release.version.to_string(), "target": release.target, "source_commit": release.source_commit, "update_protocol": 1, "data_epoch": 1});
        for dir in [&package, &copy] {
            for file in [
                "README.md",
                "RUNTIME-REQUIREMENTS.md",
                "sidecars/package.json",
                "sidecars/package-lock.json",
                "sidecars/codex-manager.mjs",
                "scripts/install-node-service.sh",
                "docs/node.md",
                "docs/releasing.md",
            ] {
                fs::create_dir_all(dir.join(file).parent().unwrap()).unwrap();
                fs::write(dir.join(file), b"test fixture").unwrap();
            }
            fs::write(
                dir.join("BUILD-INFO.json"),
                serde_json::to_vec(&build).unwrap(),
            )
            .unwrap();
            fs::write(
                dir.join("vibeface-node"),
                b"#!/bin/sh\nprintf 'vibeface-node 0.0.2\\n'\n",
            )
            .unwrap();
            fs::set_permissions(dir.join("vibeface-node"), fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        verify_build(&package, &release).unwrap();
        verify_binary(&package.join("vibeface-node"), &release.version)
            .await
            .unwrap();
        assert!(tree(&package).unwrap() == tree(&copy).unwrap());
        let dependencies = copy.join("sidecars/node_modules");
        fs::create_dir(&dependencies).unwrap();
        fs::write(dependencies.join("generated.js"), b"npm ci output").unwrap();
        assert!(tree(&package).unwrap() == tree(&copy).unwrap());
        fs::remove_dir_all(&dependencies).unwrap();
        symlink(&package, &dependencies).unwrap();
        assert!(tree(&copy).is_err());
        fs::remove_file(&dependencies).unwrap();
        fs::write(&dependencies, b"not a directory").unwrap();
        assert!(tree(&copy).is_err());
        fs::remove_file(&dependencies).unwrap();
        fs::create_dir(package.join("sidecars/node_modules")).unwrap();
        assert!(verify_build(&package, &release).is_err());
        fs::remove_dir(package.join("sidecars/node_modules")).unwrap();
        fs::write(copy.join("sidecars/codex-manager.mjs"), b"corrupt").unwrap();
        assert!(tree(&package).unwrap() != tree(&copy).unwrap());
        symlink(package.join("README.md"), copy.join("unexpected-link")).unwrap();
        assert!(tree(&copy).is_err());
        let mut old_build = build.clone();
        old_build.as_object_mut().unwrap().remove("update_protocol");
        fs::write(
            package.join("BUILD-INFO.json"),
            serde_json::to_vec(&old_build).unwrap(),
        )
        .unwrap();
        assert!(verify_build(&package, &release).is_err());
        let mut future_build = build.clone();
        future_build["data_epoch"] = serde_json::json!(2);
        fs::write(
            package.join("BUILD-INFO.json"),
            serde_json::to_vec(&future_build).unwrap(),
        )
        .unwrap();
        assert!(verify_build(&package, &release).is_err());
        assert!(
            verify_binary(&package.join("vibeface-node"), &Version::new(0, 0, 3))
                .await
                .is_err()
        );
    }
}
