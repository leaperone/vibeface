use super::{Access, CLIENT_NOTIFICATIONS, CLIENT_REQUESTS, SERVER_NOTIFICATIONS, SERVER_REQUESTS};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::{env, fs};

const SCHEMA: &str = include_str!("../../schema/app-server-methods.json");
const DESCRIPTIONS: &str = include_str!("../../tests/fixtures/codex-method-descriptions.tsv");
const TEMPLATE: &str = include_str!("../../tests/fixtures/codex-method-reference.md");
const DOC_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../packages/docs/codex-app-server.md"
);
const DOC: &str = include_str!("../../../../packages/docs/codex-app-server.md");
const REGENERATE: &str = "VIBEFACE_REGENERATE_CODEX_METHOD_REFERENCE=1 cargo test --locked \
    -p vibeface-bridge --lib methods::reference::regenerate_method_reference -- --ignored --exact";

#[derive(Deserialize)]
struct Schema {
    codex_version: String,
    client_requests: Vec<String>,
    client_notifications: Vec<String>,
    server_requests: Vec<String>,
    server_notifications: Vec<String>,
}

fn parse_descriptions(text: &str) -> Result<BTreeMap<&str, &str>, String> {
    let mut descriptions = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let (method, description) = line
            .split_once('\t')
            .filter(|(method, description)| {
                !method.is_empty() && !description.trim().is_empty() && !description.contains('\t')
            })
            .ok_or_else(|| {
                format!(
                    "expected a method and description on TSV line {}",
                    index + 1
                )
            })?;
        if descriptions.insert(method, description).is_some() {
            return Err(format!("duplicate description for {method}"));
        }
    }
    Ok(descriptions)
}

fn render_table(
    group: &str,
    schema_methods: &[String],
    classified_methods: impl IntoIterator<Item = (&'static str, Option<String>)>,
    headers: &[&str],
    descriptions: &mut BTreeMap<&str, &str>,
) -> Result<String, String> {
    let mut classified = BTreeMap::new();
    for (method, label) in classified_methods {
        if classified.insert(method, label).is_some() {
            return Err(format!("{group}: duplicate classification for {method}"));
        }
    }

    let mut table = format!(
        "| {} |\n|{}",
        headers.join(" | "),
        "---|".repeat(headers.len())
    );
    let mut seen = BTreeSet::new();
    for method in schema_methods {
        if !seen.insert(method) {
            return Err(format!("{group}: duplicate schema method {method}"));
        }
        let label = classified
            .remove(method.as_str())
            .ok_or_else(|| format!("{group}: {method} is not classified"))?;
        let description = descriptions
            .remove(method.as_str())
            .ok_or_else(|| format!("{group}: missing description for {method}"))?;
        write!(table, "\n| `{method}` | ").unwrap();
        if let Some(label) = label {
            write!(table, "{label} | ").unwrap();
        }
        write!(table, "{} |", description.replace('|', "\\|")).unwrap();
    }
    if !classified.is_empty() {
        return Err(format!(
            "{group}: classified methods absent from schema: {:?}",
            classified.keys().collect::<Vec<_>>()
        ));
    }
    Ok(table)
}

fn render_reference(schema: &str, descriptions: &str) -> Result<String, String> {
    let schema: Schema = serde_json::from_str(schema).map_err(|err| err.to_string())?;
    if schema.codex_version.trim().is_empty() {
        return Err("expected a Codex version in the schema".into());
    }
    let mut descriptions = parse_descriptions(descriptions)?;
    let sections = [
        (
            "summary",
            format!(
                "Codex `{}` 包含 {} 个客户端请求、{} 个客户端通知、{} 个服务端请求和 {} 个服务端通知。",
                schema.codex_version,
                schema.client_requests.len(),
                schema.client_notifications.len(),
                schema.server_requests.len(),
                schema.server_notifications.len(),
            ),
        ),
        (
            "client_requests",
            render_table(
                "client_requests",
                &schema.client_requests,
                CLIENT_REQUESTS.iter().map(|&(method, access)| {
                    let label = match access {
                        Access::Read => "只读",
                        Access::Write => "写",
                        Access::Hidden => "隐藏",
                    };
                    (method, Some(label.to_owned()))
                }),
                &["方法", "分类", "处理与展示"],
                &mut descriptions,
            )?,
        ),
        (
            "client_notifications",
            render_table(
                "client_notifications",
                &schema.client_notifications,
                CLIENT_NOTIFICATIONS.iter().map(|&method| (method, None)),
                &["方法", "处理"],
                &mut descriptions,
            )?,
        ),
        (
            "server_requests",
            render_table(
                "server_requests",
                &schema.server_requests,
                SERVER_REQUESTS
                    .iter()
                    .map(|&(method, reply)| (method, Some(format!("{reply:?}")))),
                &["方法", "兜底策略", "兜底回复"],
                &mut descriptions,
            )?,
        ),
        (
            "server_notifications",
            render_table(
                "server_notifications",
                &schema.server_notifications,
                SERVER_NOTIFICATIONS
                    .iter()
                    .map(|&(method, category)| (method, Some(format!("{category:?}")))),
                &["方法", "类别", "进入观察状态的方式"],
                &mut descriptions,
            )?,
        ),
    ];
    if !descriptions.is_empty() {
        return Err(format!(
            "descriptions absent from schema: {:?}",
            descriptions.keys().collect::<Vec<_>>()
        ));
    }

    let mut doc = TEMPLATE.to_owned();
    for (key, section) in sections {
        let placeholder = format!("{{{{{key}}}}}");
        if doc.matches(&placeholder).count() != 1 {
            return Err(format!(
                "expected exactly one {placeholder} in the template"
            ));
        }
        doc = doc.replacen(&placeholder, &section, 1);
    }
    Ok(doc)
}

#[test]
fn committed_method_reference_matches() {
    let expected = render_reference(SCHEMA, DESCRIPTIONS).unwrap();
    assert!(
        DOC == expected,
        "packages/docs/codex-app-server.md is stale. Regenerate with:\n{REGENERATE}"
    );
}

#[test]
fn methods_must_be_classified_in_their_protocol_group() {
    let mut schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
    schema["client_requests"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("future/method"));
    assert_eq!(
        render_reference(&schema.to_string(), DESCRIPTIONS).unwrap_err(),
        "client_requests: future/method is not classified"
    );
    let methods = schema["client_requests"].as_array_mut().unwrap();
    let duplicated = methods[0].clone();
    *methods.last_mut().unwrap() = duplicated.clone();
    assert_eq!(
        render_reference(&schema.to_string(), DESCRIPTIONS).unwrap_err(),
        format!(
            "client_requests: duplicate schema method {}",
            duplicated.as_str().unwrap()
        )
    );

    let classified = [("example", None), ("example", None)];
    assert_eq!(
        render_table("group", &[], classified, &[], &mut BTreeMap::new()).unwrap_err(),
        "group: duplicate classification for example"
    );
}

#[test]
fn methods_require_one_description_each() {
    let schema: Schema = serde_json::from_str(SCHEMA).unwrap();
    let method = &schema.client_requests[0];
    let line = DESCRIPTIONS
        .lines()
        .find(|line| {
            line.split_once('\t')
                .is_some_and(|(name, _)| name == method.as_str())
        })
        .unwrap();
    let missing = DESCRIPTIONS.replace(&format!("{line}\n"), "");
    assert_eq!(
        render_reference(SCHEMA, &missing).unwrap_err(),
        format!("client_requests: missing description for {method}")
    );
    assert_eq!(
        render_reference(SCHEMA, &format!("{DESCRIPTIONS}{line}\n")).unwrap_err(),
        format!("duplicate description for {method}")
    );
    assert_eq!(
        render_reference(SCHEMA, &format!("{DESCRIPTIONS}future/method\t说明\n")).unwrap_err(),
        "descriptions absent from schema: [\"future/method\"]"
    );

    let reordered = DESCRIPTIONS.lines().rev().collect::<Vec<_>>().join("\n");
    assert_eq!(
        render_reference(SCHEMA, &reordered).unwrap(),
        render_reference(SCHEMA, DESCRIPTIONS).unwrap()
    );
}

#[test]
#[ignore = "writes the generated Markdown with an explicit regeneration flag"]
fn regenerate_method_reference() {
    if matches!(
        env::var("VIBEFACE_REGENERATE_CODEX_METHOD_REFERENCE").as_deref(),
        Ok("1")
    ) {
        let doc = render_reference(SCHEMA, DESCRIPTIONS).unwrap();
        fs::write(DOC_PATH, doc).unwrap();
    }
}
