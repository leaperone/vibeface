use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::{env, fs, path::Path};

fn main() {
    let manifest = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).to_path_buf();
    let verified = std::process::Command::new("python3")
        .arg(manifest.join("../../scripts/build-inputs.py"))
        .args(["check", "web", "--cargo"])
        .output()
        .expect("Python 3 is required to verify generated Web assets");
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    print!("{}", String::from_utf8(verified.stdout).unwrap());
    let dist = manifest.join("web/dist/client");
    println!("cargo:rerun-if-changed=web/dist/client");
    let shell = dist.join("_shell.html");
    assert!(
        shell.is_file(),
        "web client is missing at {}. Run scripts/build-web.sh before cargo build",
        shell.display()
    );
    let mut files = Vec::new();
    collect(&dist, &dist, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut source = String::from(
        "pub struct WebFile {\n    pub content_type: &'static str,\n    pub body: &'static [u8],\n}\n\npub fn get(path: &str) -> Option<WebFile> {\n    match path {\n",
    );
    for (url, absolute, content_type) in &files {
        source.push_str(&format!(
            "        {url:?} => Some(WebFile {{ content_type: {content_type:?}, body: include_bytes!({absolute:?}) }}),\n"
        ));
    }
    source.push_str("        _ => None,\n    }\n}\n");
    let shell_html = fs::read_to_string(&shell).unwrap();
    let mut policy = String::from("default-src 'self'; script-src 'self' 'wasm-unsafe-eval'");
    for hash in inline_script_hashes(&shell_html) {
        policy.push(' ');
        policy.push_str(&hash);
    }
    // Radix's scroll lock injects runtime CSS; scripts remain restricted to self and hashes.
    policy.push_str("; style-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:; img-src 'self' data: blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'");
    source.push_str(&format!(
        "pub const CONTENT_SECURITY_POLICY: &str = {policy:?};\n"
    ));
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("web_assets.rs"),
        source,
    )
    .unwrap();
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, String, &'static str)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        println!("cargo:rerun-if-changed={}", path.display());
        assert!(!path.is_symlink(), "web assets must not contain symlinks");
        if path.is_dir() {
            collect(root, &path, files);
            continue;
        }
        let url = format!(
            "/{}",
            path.strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        );
        files.push((
            url,
            path.to_string_lossy().into_owned(),
            content_type(&path),
        ));
    }
}

fn inline_script_hashes(html: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    let mut rest = html;
    while let Some((_, script)) = rest.split_once("<script") {
        let (open, content) = script.split_once('>').expect("unclosed script tag");
        let (body, after) = content.split_once("</script>").expect("unclosed script");
        if !open.contains("src=") {
            hashes.push(format!(
                "'sha256-{}'",
                STANDARD.encode(Sha256::digest(body))
            ));
        }
        rest = after;
    }
    hashes
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
