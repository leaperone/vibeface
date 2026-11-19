//! Transport-neutral validation shared by local tool callers.
//!
//! This is intentionally only the pre-authentication boundary needed by the CLI. Core remains
//! authoritative for each tool's complete input schema and invocation checks.

use anyhow::{Context, Result, ensure};
use serde_json::Value;

/// Apply the local tool-call preflight before a caller reads credentials or contacts a Node.
///
/// The checks here are deliberately a subset of Core's schema validation. They preserve the CLI
/// contract for malformed JSON/object arguments, call reference injection and mismatches, and
/// the shape required for the locally orchestrated `dispatch_work` path.
pub fn prepare_tool_arguments(
    name: &str,
    arguments: &mut Value,
    call_ref: Option<&str>,
) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 128, "invalid tool name");
    ensure!(
        arguments.is_object(),
        "tool arguments must be a JSON object"
    );
    let argument_ref = arguments["call_ref"].as_str();
    ensure!(
        call_ref
            .zip(argument_ref)
            .is_none_or(|(flag, argument)| flag == argument),
        "--call-ref must match arguments.call_ref"
    );
    let call_ref = call_ref.or(argument_ref).map(str::to_owned);
    if let Some(call_ref) = &call_ref {
        arguments["call_ref"] = Value::String(call_ref.clone());
    }
    if name != "dispatch_work" {
        return Ok(());
    }
    let call_id = call_ref
        .as_deref()
        .context("dispatch_work requires --call-ref or arguments.call_ref")?;
    vibeface_protocol::validate_id(call_id)
        .map_err(|error| anyhow::anyhow!("arguments.call_ref is invalid: {error}"))?;
    ensure!(
        arguments["prompt"]
            .as_str()
            .is_some_and(|prompt| !prompt.is_empty()),
        "dispatch_work requires a prompt"
    );
    ensure!(
        arguments["thread"]
            .as_str()
            .is_some_and(|thread| !thread.is_empty())
            || (arguments["title"]
                .as_str()
                .is_some_and(|title| !title.is_empty())
                && arguments["cwd"].as_str().is_some_and(|cwd| !cwd.is_empty())),
        "dispatch_work requires thread or title and cwd"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_malformed_preflight_arguments() {
        for (name, arguments, call_ref, expected) in [
            (
                "tool",
                json!(null),
                None,
                "tool arguments must be a JSON object",
            ),
            ("", json!({}), None, "invalid tool name"),
            (
                "tool",
                json!({"call_ref":"argument"}),
                Some("flag"),
                "--call-ref must match arguments.call_ref",
            ),
            (&"x".repeat(129), json!({}), None, "invalid tool name"),
            (
                "dispatch_work",
                json!({"prompt":"p","thread":"t"}),
                None,
                "dispatch_work requires --call-ref or arguments.call_ref",
            ),
            (
                "dispatch_work",
                json!({"prompt":"p","thread":"t","call_ref":"bad ref"}),
                None,
                "arguments.call_ref is invalid",
            ),
            (
                "dispatch_work",
                json!({"call_ref":"valid-ref","thread":"t"}),
                None,
                "dispatch_work requires a prompt",
            ),
            (
                "dispatch_work",
                json!({"call_ref":"valid-ref","prompt":"p"}),
                None,
                "dispatch_work requires thread or title and cwd",
            ),
        ] {
            let mut arguments = arguments;
            let error = prepare_tool_arguments(name, &mut arguments, call_ref)
                .expect_err("malformed arguments must fail preflight");
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn accepts_and_normalizes_valid_arguments() -> Result<()> {
        let mut arguments = json!({"prompt":"p","title":"t","cwd":"/work"});
        prepare_tool_arguments("dispatch_work", &mut arguments, Some("call-1"))?;
        assert_eq!(arguments["call_ref"], "call-1");

        let mut arguments = json!({"prompt":"p","thread":"thread-1","call_ref":"call-2"});
        prepare_tool_arguments("dispatch_work", &mut arguments, None)?;
        assert_eq!(arguments["call_ref"], "call-2");

        let mut arguments = json!({"anything":true});
        prepare_tool_arguments("read_only", &mut arguments, Some("call-3"))?;
        assert_eq!(arguments["call_ref"], "call-3");
        Ok(())
    }
}
