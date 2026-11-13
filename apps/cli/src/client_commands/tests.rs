use super::*;
use clap::Parser;

#[derive(Parser)]
struct TestCli {
    #[command(subcommand)]
    command: BusinessCommand,
}

fn parse(arguments: &[&str]) -> Result<Request> {
    TestCli::try_parse_from(std::iter::once("vibeface").chain(arguments.iter().copied()))?
        .command
        .into_request()
}

fn read_json(request: Request) -> Value {
    match request {
        Request::Read(request) => serde_json::to_value(request).unwrap(),
        Request::Write(_) => panic!("expected read request"),
    }
}

fn write_json(request: Request) -> Value {
    match request {
        Request::Write(action) => serde_json::to_value(action).unwrap(),
        Request::Read(_) => panic!("expected write action"),
    }
}

#[test]
fn no_input_reads_and_native_history_defaults_are_usable() {
    assert_eq!(
        read_json(parse(&["read", "home"]).unwrap()),
        json!({"type":"home"})
    );
    assert_eq!(
        read_json(
            parse(&[
                "read",
                "conversation-history",
                "--input",
                r#"{"conversation_id":"c","section":"messages"}"#
            ])
            .unwrap()
        ),
        json!({"type":"conversation_history","conversation_id":"c","section":"messages","cursor":null,"limit":50})
    );
    assert_eq!(
        read_json(
            parse(&[
                "read",
                "diagnostics-filtered",
                "--input",
                r#"{"since_ms":12,"until_ms":34}"#
            ])
            .unwrap()
        ),
        json!({"type":"diagnostics_filtered","since_ms":12,"until_ms":34})
    );
}

#[test]
fn nested_workspace_delivery_and_unanswered_operations_keep_their_native_shape() {
    assert_eq!(
        write_json(parse(&["write", "workspace", "--input", r#"{"command":{"kind":"control_input","message_id":"m","expected_event":"event","action":{"kind":"move","before_message_id":"next"}}}"#]).unwrap()),
        json!({"type":"workspace","command":{"kind":"control_input","message_id":"m","expected_event":"event","action":{"kind":"move","before_message_id":"next"}}})
    );
    assert_eq!(
        write_json(
            parse(&[
                "write",
                "delivery",
                "--input",
                r#"{"op":"confirm_batch","intent_ids":["a","b"]}"#
            ])
            .unwrap()
        ),
        json!({"type":"delivery","op":"confirm_batch","intent_ids":["a","b"]})
    );
    assert_eq!(
        write_json(
            parse(&[
                "write",
                "unanswered",
                "--input",
                r#"{"conversation_id":"c","message_ids":["m"],"action":"dismiss"}"#
            ])
            .unwrap()
        ),
        json!({"type":"unanswered","conversation_id":"c","message_ids":["m"],"action":"dismiss"})
    );
}

#[test]
fn input_keeps_stable_id_and_zero_sequence_for_sdk_allocation() {
    assert_eq!(
        write_json(parse(&["write", "input", "--input", r#"{"conversation_id":"c","input":{"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"text":"Later","report_results":false}}"#]).unwrap()),
        json!({"type":"input","conversation_id":"c","input":{"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"text":"Later","report_results":false}})
    );
}

#[test]
fn malformed_payloads_fail_before_connection_without_echoing_private_values() {
    for payload in [
        r#"{"conversation_id":"c","type":"home"}"#,
        r#"{"conversation_id":"c","unexpected":"PRIVATE"}"#,
        r#"{"conversation_id":"PRIVATE""#,
        r#"["PRIVATE"]"#,
        r#"{}"#,
    ] {
        let error = parse(&["read", "conversation", "--input", payload])
            .err()
            .unwrap();
        assert!(!error.to_string().contains("PRIVATE"));
    }
    assert_eq!(
        parse(&["read", "home", "--input", r#"{"extra":true}"#])
            .err()
            .unwrap()
            .to_string(),
        "invalid home payload: unknown field; see this operation's --help"
    );
    assert_eq!(
        parse(&["read", "home", "--input", r#"{"type":"home"}"#])
            .err()
            .unwrap()
            .to_string(),
        "omit the outer type field; the named command selects the operation"
    );
}

#[test]
fn nested_unknown_fields_and_invalid_delivery_batches_are_rejected() {
    for (operation, payload) in [
        (
            "workspace",
            r#"{"command":{"kind":"control_task","task_id":"t","expected_revision":1,"action":"stop","typo":true}}"#,
        ),
        (
            "workspace",
            r#"{"command":{"kind":"control_input","message_id":"m","expected_event":"e","action":{"kind":"release","typo":true}}}"#,
        ),
        (
            "workspace",
            r#"{"command":{"kind":"control_input","message_id":"m","expected_event":"e","action":{"kind":"cancel","typo":null}}}"#,
        ),
        (
            "delivery",
            r#"{"op":"confirm","intent_id":"i","typo":true}"#,
        ),
        ("delivery", r#"{"op":"confirm_batch","intent_ids":["a"]}"#),
        ("delivery", r#"{"op":"not_an_operation"}"#),
        (
            "input",
            r#"{"conversation_id":"c","input":{"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"text":"Later","report_results":false,"typo":true}}"#,
        ),
        (
            "node-settings",
            r#"{"roots":[],"manager":{"provider":"codex","workspace":"/workspace","model":"test","reasoning_effort":null,"typo":true}}"#,
        ),
    ] {
        assert!(
            parse(&["write", operation, "--input", payload]).is_err(),
            "accepted {operation} malformed payload"
        );
    }
}

#[test]
fn unit_input_controls_use_the_exact_native_document() {
    assert_eq!(
        write_json(parse(&["write", "workspace", "--input", r#"{"command":{"kind":"control_input","message_id":"m","expected_event":"e","action":{"kind":"release"}}}"#]).unwrap()),
        json!({"type":"workspace","command":{"kind":"control_input","message_id":"m","expected_event":"e","action":{"kind":"release"}}})
    );
    assert_eq!(
        write_json(parse(&["write", "workspace", "--input", r#"{"command":{"kind":"control_input","message_id":"m","expected_event":"e","action":{"kind":"cancel"}}}"#]).unwrap()),
        json!({"type":"workspace","command":{"kind":"control_input","message_id":"m","expected_event":"e","action":{"kind":"cancel"}}})
    );
    assert_eq!(
        parse(&["write", "workspace", "--input", r#"{"command":{"kind":"control_input","message_id":"m","expected_event":"e","action":{"kind":"release","private":"DO_NOT_ECHO"}}}"#])
            .err().unwrap().to_string(),
        "invalid workspace payload: unknown field; see this operation's --help"
    );
    assert_eq!(
        write_json(parse(&["write", "workspace", "--input", r#"{"command":{"kind":"control_input","message_id":"m","expected_event":"e","action":{"kind":"move","before_message_id":null}}}"#]).unwrap()),
        json!({"type":"workspace","command":{"kind":"control_input","message_id":"m","expected_event":"e","action":{"kind":"move"}}})
    );
}

#[test]
fn flattened_input_preserves_null_revision_and_empty_attachments() {
    assert_eq!(
        write_json(parse(&["write", "input", "--input", r#"{"conversation_id":"c","input":{"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"expected_revision":null,"text":"Later","attachments":[],"report_results":false}}"#]).unwrap()),
        json!({"type":"input","conversation_id":"c","input":{"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"text":"Later","report_results":false}})
    );
    assert_eq!(
        write_json(parse(&["write", "input", "--input", r#"{"conversation_id":"c","input":{"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"continue","sequence":0,"expected_revision":3,"text":"Read this","attachments":[{"hash":"fixture","size":1,"name":"a.txt"}],"report_results":true}}"#]).unwrap()),
        json!({"type":"input","conversation_id":"c","input":{"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"continue","sequence":0,"expected_revision":3,"text":"Read this","attachments":[{"hash":"fixture","size":1,"name":"a.txt"}],"report_results":true}})
    );
    assert_eq!(
        parse(&["write", "input", "--input", r#"{"conversation_id":"c","input":{"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"expected_revision":null,"text":"Later","attachments":[],"report_results":false,"typo":null}}"#]).err().unwrap().to_string(),
        "invalid input payload: unknown field; see this operation's --help"
    );
}

#[test]
fn catalog_payloads_reject_unknown_fields_at_each_exposed_tagged_boundary() {
    let capabilities = capabilities();
    for operation in capabilities["operations"].as_array().unwrap() {
        let command = operation["command"].as_str().unwrap();
        let mut input = operation["example"].clone();
        input["unexpected_cli_field"] = Value::Null;
        let input = input.to_string();
        let mut arguments: Vec<_> = command.split_whitespace().skip(1).collect();
        arguments.extend(["--input", &input]);
        assert!(
            parse(&arguments).is_err(),
            "accepted unknown field on {command}"
        );
    }
    for name in [
        "DeliveryRead",
        "DeliveryAction",
        "WorkspaceCommand",
        "InputTarget",
        "InputControl",
    ] {
        for variant in capabilities["nested"][name]["variants"].as_array().unwrap() {
            let mut example = variant["example"].clone();
            example["unexpected_cli_field"] = Value::Null;
            let (kind, operation, input) = match name {
                "DeliveryRead" => ("read", "delivery", example),
                "DeliveryAction" => ("write", "delivery", example),
                "WorkspaceCommand" => ("write", "workspace", json!({"command":example})),
                "InputTarget" => (
                    "read",
                    "context",
                    json!({"conversation_id":"c","target":example}),
                ),
                "InputControl" => (
                    "write",
                    "workspace",
                    json!({"command":{"kind":"control_input","message_id":"m","expected_event":"e","action":example}}),
                ),
                _ => unreachable!(),
            };
            assert!(
                parse(&[kind, operation, "--input", &input.to_string()]).is_err(),
                "accepted unknown field on {name}::{}",
                variant["native_variant"]
            );
        }
    }
}

#[test]
fn unknown_operations_and_overlarge_input_are_rejected() {
    assert!(TestCli::try_parse_from(["vibeface", "read", "arbitrary_rpc"]).is_err());
    assert!(TestCli::try_parse_from(["vibeface", "write", "call"]).is_err());
    let large = " ".repeat(MAX_INPUT_BYTES + 1);
    assert_eq!(
        parse(&["read", "home", "--input", &large])
            .err()
            .unwrap()
            .to_string(),
        "operation input exceeds 1 MiB"
    );
    assert_eq!(
        bounded_input(large.as_bytes()).unwrap_err().to_string(),
        "operation input exceeds 1 MiB"
    );
}

#[test]
fn file_input_uses_the_same_typed_parser() {
    let path = std::env::temp_dir().join(format!(
        "vibeface-command-input-{}.json",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, br#"{"provider":"codex"}"#).unwrap();
    let input = format!("@{}", path.display());
    let result = parse(&["read", "provider-models", "--input", &input]);
    std::fs::remove_file(path).unwrap();
    assert_eq!(
        read_json(result.unwrap()),
        json!({"type":"provider_models","provider":"codex"})
    );
}

#[test]
fn endpoint_secret_changes_preserve_keep_clear_and_set() {
    for (input, expected_endpoint) in [
        (
            r#"{"provider":"opencode","connect_local":false,"env":{},"opencode_endpoint":null}"#,
            Value::Null,
        ),
        (
            r#"{"provider":"opencode","connect_local":false,"env":{},"opencode_endpoint":{"protocol":"openai_compatible","base_url":"https://example.invalid/v1","models":["fixture"],"api_key":null}}"#,
            json!({"protocol":"openai_compatible","base_url":"https://example.invalid/v1","models":["fixture"],"api_key":null}),
        ),
        (
            r#"{"provider":"opencode","connect_local":false,"env":{},"opencode_endpoint":{"protocol":"openai","base_url":"https://example.invalid/v1","models":["fixture"],"api_key":"fixture-only"}}"#,
            json!({"protocol":"openai","base_url":"https://example.invalid/v1","models":["fixture"],"api_key":"fixture-only"}),
        ),
    ] {
        assert_eq!(
            write_json(parse(&["write", "provider-settings", "--input", input]).unwrap()),
            json!({"type":"provider_settings","provider":"opencode","connect_local":false,"env":{},"opencode_endpoint":expected_endpoint})
        );
    }
    assert_eq!(
        write_json(
            parse(&[
                "write",
                "provider-settings",
                "--input",
                r#"{"provider":"opencode","connect_local":false,"env":{}}"#
            ])
            .unwrap()
        ),
        json!({"type":"provider_settings","provider":"opencode","connect_local":false,"env":{}})
    );
}

#[test]
fn published_operation_examples_parse_through_the_actual_cli() {
    for operation in capabilities()["operations"].as_array().unwrap() {
        let command = operation["command"].as_str().unwrap();
        let input = serde_json::to_string(&operation["example"]).unwrap();
        let mut arguments: Vec<_> = command.split_whitespace().skip(1).collect();
        arguments.extend(["--input", &input]);
        assert!(parse(&arguments).is_ok(), "unusable example for {command}");
    }
}

#[test]
fn published_nested_examples_deserialize_as_the_authoritative_native_types() {
    let nested = capabilities()["nested"].clone();
    for example in nested["DeliveryRead"]["variants"].as_array().unwrap() {
        assert!(
            serde_json::from_value::<vibeface_client::DeliveryRead>(example["example"].clone())
                .is_ok()
        );
    }
    for example in nested["DeliveryAction"]["variants"].as_array().unwrap() {
        assert!(
            serde_json::from_value::<vibeface_client::DeliveryAction>(example["example"].clone())
                .is_ok()
        );
    }
    for example in nested["WorkspaceCommand"]["variants"].as_array().unwrap() {
        assert!(
            serde_json::from_value::<vibeface_client::wire::WorkspaceCommand>(
                example["example"].clone()
            )
            .is_ok()
        );
    }
    for example in nested["InputTarget"]["variants"].as_array().unwrap() {
        assert!(
            serde_json::from_value::<vibeface_client::wire::InputTarget>(
                example["example"].clone()
            )
            .is_ok()
        );
    }
}

#[test]
fn operation_help_shows_usable_examples() {
    let error = TestCli::try_parse_from(["vibeface", "write", "workspace", "--help"])
        .err()
        .unwrap();
    let help = error.to_string();
    assert!(help.contains("control_input"));
    assert!(help.contains("vibeface client write workspace --input"));
    assert!(help.contains("expected_revision"));
}
