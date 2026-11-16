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

    fn parse<T: DeserializeOwned>(self, tag: &str) -> Result<T> {
        parse_document(self.document()?, tag)
    }
}

fn parse_document<T: DeserializeOwned>(mut document: Value, tag: &str) -> Result<T> {
    let object = document
        .as_object_mut()
        .context("operation input must be a JSON object")?;
    ensure!(
        !object.contains_key("type"),
        "omit the outer type field; the named command selects the operation"
    );
    object.insert("type".into(), Value::String(tag.into()));
    serde_json::from_value(document).map_err(|error| {
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
    })
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
