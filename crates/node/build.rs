use sha2::{Digest, Sha256};
use std::{env, fs, path::Path};

fn main() {
    let manifest = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).to_path_buf();
    let dist = manifest.join("web/dist/client");
    println!("cargo:rerun-if-changed=web/dist/client");
    let shell = dist.join("_shell.html");
    if !shell.is_file() {
        panic!(
            "web client is missing at {}. Run scripts/build-web.sh before cargo build",
            shell.display()
        );
    }

    let mut files = Vec::new();
    collect(&dist, &dist, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut source = String::from(
        "pub struct WebFile {\n    pub content_type: &'static str,\n    pub body: &'static [u8],\n}\n\n#[cfg(test)]\npub fn paths() -> &'static [&'static str] {\n    &[\n",
    );
    for (url, _, _) in &files {
        source.push_str(&format!("        {url:?},\n"));
    }
    source.push_str("    ]\n}\n\npub fn get(path: &str) -> Option<WebFile> {\n    match path {\n");
    for (url, absolute, content_type) in &files {
        source.push_str(&format!(
            "        {url:?} => Some(WebFile {{ content_type: {content_type:?}, body: include_bytes!({absolute:?}) }}),\n"
        ));
    }
    source.push_str("        _ => None,\n    }\n}\n");
    let shell_html = fs::read_to_string(&shell).unwrap();
    let hashes = inline_script_hashes(&shell_html);
    let mut policy = String::from("default-src 'self'; script-src 'self'");
    for hash in &hashes {
        policy.push(' ');
        policy.push_str(hash);
    }
    policy.push_str("; style-src 'self'; connect-src 'self'; img-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'");
    source.push_str(&format!(
        "pub const CONTENT_SECURITY_POLICY: &str = {policy:?};\n"
    ));
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("web_assets.rs");
    fs::write(out, source).unwrap();
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, String, String)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        println!("cargo:rerun-if-changed={}", path.display());
        if path.is_dir() {
            collect(root, &path, files);
            continue;
        }
        let relative = path.strip_prefix(root).unwrap();
        let url = format!("/{}", relative.to_string_lossy().replace('\\', "/"));
        files.push((
            url,
            path.to_string_lossy().into_owned(),
            content_type(&path),
        ));
    }
}

fn inline_script_hashes(html: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    let bytes = html.as_bytes();
    let mut rest = 0;
    while let Some(found) = html[rest..].find("<script") {
        let start = rest + found;
        let Some(tag_end) = html[start..].find('>') else {
            break;
        };
        let open = &html[start..start + tag_end];
        let content_at = start + tag_end + 1;
        let Some(close_rel) = html[content_at..].find("</script>") else {
            break;
        };
        if !open.contains("src=") {
            let content = &bytes[content_at..content_at + close_rel];
            let digest = Sha256::digest(content);
            hashes.push(format!("'sha256-{}'", base64(&digest)));
        }
        rest = content_at + close_rel + "</script>".len();
    }
    hashes
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let value = ((bytes[index] as u32) << 16)
            | ((bytes[index + 1] as u32) << 8)
            | bytes[index + 2] as u32;
        out.push(ALPHABET[((value >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((value >> 12) & 63) as usize] as char);
        out.push(ALPHABET[((value >> 6) & 63) as usize] as char);
        out.push(ALPHABET[(value & 63) as usize] as char);
        index += 3;
    }
    if index < bytes.len() {
        let mut value = (bytes[index] as u32) << 16;
        out.push(ALPHABET[((value >> 18) & 63) as usize] as char);
        if index + 1 < bytes.len() {
            value |= (bytes[index + 1] as u32) << 8;
            out.push(ALPHABET[((value >> 12) & 63) as usize] as char);
            out.push(ALPHABET[((value >> 6) & 63) as usize] as char);
            out.push('=');
        } else {
            out.push(ALPHABET[((value >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        }
    }
    out
}

fn content_type(path: &Path) -> String {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("json" | "map") => "application/json",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
    .to_string()
}
