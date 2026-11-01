use anyhow::{Context, Result, ensure};
use axum::{body::Bytes, http::HeaderValue};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub(crate) const DEFAULT_CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:; img-src 'self' data: blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

#[derive(Clone)]
pub(crate) struct StaticAssets {
    files: BTreeMap<String, StaticFile>,
    pub(crate) content_security_policy: HeaderValue,
}

#[derive(Debug, Deserialize)]
struct Receipt {
    version: u8,
    component: String,
    content_security_policy: String,
    outputs: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub(crate) struct StaticFile {
    pub(crate) content_type: &'static str,
    pub(crate) body: Bytes,
}

impl StaticAssets {
    pub(crate) fn load(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let metadata = fs::symlink_metadata(&root)
            .with_context(|| format!("static App directory is missing: {}", root.display()))?;
        ensure!(metadata.is_dir(), "static App path is not a directory");
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot resolve static App directory: {}", root.display()))?;
        reject_symlinks(&root)?;

        let receipt_path = root.join(".vibeface-build.json");
        let receipt: Receipt =
            serde_json::from_slice(&fs::read(&receipt_path).with_context(|| {
                format!("static App receipt is missing: {}", receipt_path.display())
            })?)
            .with_context(|| format!("invalid static App receipt: {}", receipt_path.display()))?;
        ensure!(
            receipt.version == 1,
            "unsupported static App receipt version"
        );
        ensure!(
            receipt.component == "web",
            "static App receipt is not a web build"
        );
        ensure!(
            !receipt.content_security_policy.is_empty(),
            "static App receipt has no CSP"
        );
        let files = validate_outputs(&root, &receipt.outputs)?;
        for name in [
            "_shell.html",
            "vibeface_secure.js",
            "vibeface_secure_bg.wasm",
            "_headers",
        ] {
            ensure!(
                receipt.outputs.contains_key(name),
                "static App receipt is missing {name}"
            );
            ensure!(
                files.contains_key(name),
                "static App file is missing: {name}"
            );
        }
        let content_security_policy = HeaderValue::from_str(&receipt.content_security_policy)
            .context("static App receipt has an invalid CSP")?;
        let headers = fs::read_to_string(root.join("_headers"))?;
        ensure!(
            headers == static_headers(&receipt.content_security_policy),
            "static App headers do not match the receipt CSP"
        );
        ensure!(
            !files["_shell.html"].body.is_empty(),
            "static App shell is empty"
        );
        Ok(Self {
            files,
            content_security_policy,
        })
    }

    pub(crate) fn read(&self, path: &str) -> Option<StaticFile> {
        let relative = relative_path(path)?;
        if is_metadata(relative) {
            return None;
        }
        self.files.get(relative).cloned()
    }

    pub(crate) fn is_unsafe_path(path: &str) -> bool {
        path.split('/')
            .any(|part| part == "." || part == ".." || part.contains('\\'))
            || {
                let encoded = path.to_ascii_lowercase();
                encoded.contains("%2e") || encoded.contains("%5c")
            }
    }

    pub(crate) fn shell_file(&self) -> StaticFile {
        self.files
            .get("_shell.html")
            .expect("validated App shell")
            .clone()
    }
}

fn reject_symlinks(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "static App must not contain symlinks: {}",
                path.display()
            );
            if metadata.is_dir() {
                pending.push(path);
            } else {
                ensure!(
                    metadata.is_file(),
                    "static App entry is not a regular file: {}",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_outputs(
    root: &Path,
    outputs: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, StaticFile>> {
    let mut actual = BTreeMap::new();
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if path == root.join(".vibeface-build.json") {
                continue;
            }
            let name = path
                .strip_prefix(root)
                .expect("asset path must stay below root")
                .to_string_lossy()
                .replace('\\', "/");
            let body = Bytes::from(fs::read(&path)?);
            let digest = hex::encode(Sha256::digest(&body));
            actual.insert(name.clone(), digest);
            files.insert(
                name,
                StaticFile {
                    content_type: content_type(&path),
                    body,
                },
            );
        }
    }
    ensure!(
        actual == *outputs,
        "static App output hashes do not match the receipt"
    );
    Ok(files)
}

fn relative_path(path: &str) -> Option<&str> {
    let relative = path.strip_prefix('/')?;
    if relative.is_empty()
        || relative.split('/').any(|part| part.is_empty())
        || StaticAssets::is_unsafe_path(path)
    {
        return None;
    }
    Some(relative)
}

fn is_metadata(path: &str) -> bool {
    path == ".vibeface-build.json"
        || path == "_headers"
        || path == "_redirects"
        || path.starts_with('.')
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("json" | "map") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn static_headers(policy: &str) -> String {
    format!(
        "/*\n  Content-Security-Policy: {policy}\n  X-Content-Type-Options: nosniff\n  X-Frame-Options: DENY\n  Referrer-Policy: no-referrer\n"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_CONTENT_SECURITY_POLICY, StaticAssets, content_type, is_metadata, relative_path,
        static_headers,
    };
    use sha2::{Digest, Sha256};
    use std::{collections::BTreeMap, fs, path::Path};

    fn fixture() -> std::path::PathBuf {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("static-assets-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let mut outputs = BTreeMap::new();
        let files = [
            ("_shell.html", b"<main>shell</main>".as_slice()),
            (
                "vibeface_secure.js",
                b"export default function init() {}".as_slice(),
            ),
            ("vibeface_secure_bg.wasm", b"\0asmfixture".as_slice()),
            ("known.js", b"known".as_slice()),
        ];
        for (name, body) in files {
            fs::write(root.join(name), body).unwrap();
            outputs.insert(name.to_string(), hex::encode(Sha256::digest(body)));
        }
        let headers = static_headers(DEFAULT_CONTENT_SECURITY_POLICY);
        fs::write(root.join("_headers"), headers.as_bytes()).unwrap();
        outputs.insert(
            "_headers".into(),
            hex::encode(Sha256::digest(headers.as_bytes())),
        );
        fs::write(
            root.join(".vibeface-build.json"),
            serde_json::json!({
                "version": 1,
                "component": "web",
                "content_security_policy": DEFAULT_CONTENT_SECURITY_POLICY,
                "outputs": outputs,
            })
            .to_string(),
        )
        .unwrap();
        root
    }

    #[test]
    fn loaded_assets_are_immutable_and_do_not_expose_later_files() {
        let root = fixture();
        let assets = StaticAssets::load(&root).unwrap();
        assert_eq!(assets.read("/known.js").unwrap().body.as_ref(), b"known");
        fs::write(root.join("known.js"), b"mutated").unwrap();
        fs::write(root.join("private.txt"), b"secret").unwrap();
        assert_eq!(assets.read("/known.js").unwrap().body.as_ref(), b"known");
        assert!(assets.read("/private.txt").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn static_paths_keep_metadata_and_traversal_outside_the_asset_namespace() {
        assert_eq!(relative_path("/assets/app.js"), Some("assets/app.js"));
        for path in ["/", "/../secret", "/assets/../secret", "/assets\\secret"] {
            assert!(relative_path(path).is_none());
        }
        assert!(StaticAssets::is_unsafe_path("/%2e%2e/secret"));
        for path in [
            ".vibeface-build.json",
            "_headers",
            "_redirects",
            ".git/config",
        ] {
            assert!(is_metadata(path));
        }
        assert!(!is_metadata("assets/app.js"));
    }

    #[test]
    fn static_content_types_match_browser_resources() {
        assert_eq!(
            content_type(Path::new("app.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("module.wasm")), "application/wasm");
        assert_eq!(
            content_type(Path::new("unknown")),
            "application/octet-stream"
        );
    }
}
