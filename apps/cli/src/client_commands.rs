//! Named client operations. Native SDK types remain the payload authority.
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::io::Read;
use vibeface_client::{Action, ReadRequest};

mod catalog;
pub use catalog::{ReadCommand, WriteCommand};

const MAX_INPUT_BYTES: usize = 1024 * 1024;

pub(crate) fn json_document(source: &str) -> Result<Value> {
    JsonInput {
        input: Some(source.to_owned()),
    }
    .document()
}

#[derive(Subcommand)]
pub enum BusinessCommand {
    /// Read typed business data from the selected Node.
    Read {
        #[command(subcommand)]
        operation: ReadCommand,
    },
    /// Stage and submit a typed durable business operation.
    Write {
        #[command(subcommand)]
        operation: WriteCommand,
    },
}

pub enum Request {
    Read(ReadRequest),
    Write(Action),
}

#[derive(Args)]
pub struct JsonInput {
    /// JSON object, @FILE, or - for stdin. Omit the outer type tag.
    #[arg(long, value_name = "JSON|@FILE|-")]
    input: Option<String>,
}

impl BusinessCommand {
    pub fn into_request(self) -> Result<Request> {
        match self {
            Self::Read { operation } => operation.into_request().map(Request::Read),
            Self::Write { operation } => operation.into_request().map(Request::Write),
        }
    }
}

fn bounded_input(mut reader: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("cannot read operation input")?;
    ensure!(
        bytes.len() <= MAX_INPUT_BYTES,
        "operation input exceeds 1 MiB"
    );
    Ok(bytes)
}

impl JsonInput {
    fn document(self) -> Result<Value> {
        let Some(source) = self.input else {
            return Ok(json!({}));
        };
        let bytes = if source == "-" {
            bounded_input(std::io::stdin().lock())?
        } else if let Some(path) = source.strip_prefix('@') {
            ensure!(!path.is_empty(), "@FILE requires a file path");
            bounded_input(std::fs::File::open(path).context("cannot open operation input file")?)?
        } else {
            ensure!(
                source.len() <= MAX_INPUT_BYTES,
                "operation input exceeds 1 MiB"
            );
            source.into_bytes()
        };
        serde_json::from_slice(&bytes).map_err(|error| {
            anyhow::anyhow!(
                "invalid JSON input at line {}, column {}",
                error.line(),
                error.column()
            )
        })
    }

    fn parse<T: StrictInput>(self, tag: &str) -> Result<T> {
        parse_document(self.document()?, tag)
    }
}

trait StrictInput: DeserializeOwned {
    fn validate_document(&self, document: &Value) -> Result<()>;
}

impl StrictInput for ReadRequest {
    fn validate_document(&self, _: &Value) -> Result<()> {
        Ok(())
    }
}

impl StrictInput for Action {
    fn validate_document(&self, document: &Value) -> Result<()> {
        use vibeface_client::wire::{Attachment, InputControl, WorkspaceCommand};
        match self {
            Self::Input { input, .. } => {
                // Flattened InputRoute does not make InputRequest reject extra fields.
                // Represent skipped fields to derive every allowed key from the native
                // serializer. These schema values never enter the submitted request.
                let mut shape = input.clone();
                shape.route.expected_revision = Some(0);
                shape.attachments.push(Attachment {
                    hash: String::new(),
                    size: 0,
                    name: String::new(),
                });
                let shape = serde_json::to_value(shape)?;
                let allowed = shape
                    .as_object()
                    .context("native input must serialize as an object")?;
                let supplied = document
                    .get("input")
                    .and_then(Value::as_object)
                    .context("input payload must be an object")?;
                ensure!(
                    supplied.keys().all(|key| allowed.contains_key(key)),
                    "invalid input payload: unknown field; see this operation's --help"
                );
            }
            Self::Workspace {
                command: WorkspaceCommand::ControlInput { action, .. },
            } => match action {
                // Serde ignores extra map entries on internally tagged unit variants.
                // Their native serialization defines the complete allowed payload.
                InputControl::Release | InputControl::Cancel => ensure!(
                    document.pointer("/command/action") == Some(&serde_json::to_value(action)?),
                    "invalid workspace payload: unknown field; see this operation's --help"
                ),
                InputControl::Edit { .. } | InputControl::Move { .. } => {}
            },
            _ => {}
        }
        Ok(())
    }
}

fn parse_document<T: StrictInput>(mut document: Value, tag: &str) -> Result<T> {
    let object = document
        .as_object_mut()
        .context("operation input must be a JSON object")?;
    ensure!(
        !object.contains_key("type"),
        "omit the outer type field; the named command selects the operation"
    );
    object.insert("type".into(), Value::String(tag.into()));
    let request: T = serde_json::from_value(document.clone()).map_err(|error| {
        // Serde messages can contain invalid values, including private configuration.
        let message = error.to_string();
        let reason = if message.starts_with("unknown field") {
            "unknown field"
        } else if message.starts_with("missing field") {
            "missing required field"
        } else {
            "invalid field value or nested operation"
        };
        anyhow::anyhow!("invalid {tag} payload: {reason}; see this operation's --help")
    })?;
    request.validate_document(&document)?;
    Ok(request)
}

pub fn capabilities() -> Value {
    json!({
        "schema_version": 1,
        "input_limit_bytes": MAX_INPUT_BYTES,
        "source": {
            "read_write": "crates/client/src/api.rs",
            "workspace": "crates/protocol/src/workspace.rs",
            "history": "crates/protocol/src/history.rs"
        },
        "operations": catalog::operations(),
        "nested": catalog::nested_capabilities(),
        "local_lifecycle": [{
            "command": "node rejoin",
            "core_methods": ["rejoin"],
            "authority": "local maintenance",
            "native_variant": null
        }]
    })
}

#[cfg(test)]
mod tests;
