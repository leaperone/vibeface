use crate::manage::ClientAccess;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use vibeface_client::{
    Action, Client, ClientKey, ClientReader, Endpoint, OperationSnapshot, OperationState,
    ReadRequest, wire::InputRequest,
};

const MAX_ATTACHMENT: u64 = 64 * 1024 * 1024;

pub struct Profile {
    pub key: Arc<ClientKey>,
    pub endpoint: Endpoint,
    pub directory: PathBuf,
    pub json: bool,
}

impl Profile {
    pub fn open(access: &ClientAccess) -> Result<Self> {
        let key = Arc::new(access.unlock()?);
        let endpoint = match access.entry(&key)? {
            Some(entry) => Endpoint::Gateway(entry),
            None => key.remembered_endpoint()?,
        };
        Ok(Self {
            key,
            endpoint,
            directory: access.resolved_client_dir()?,
            json: access.json,
        })
    }

    pub async fn reader(&self) -> Result<ClientReader> {
        ClientReader::open(self.key.clone(), self.endpoint.clone()).await
    }

    pub async fn writer(&self) -> Result<Client> {
        Client::open_or_offline(
            self.key.clone(),
            self.endpoint.clone(),
            self.directory.join("outbox"),
        )
        .await
    }

    pub async fn read(&self, request: ReadRequest) -> Result<Value> {
        let reader = self.reader().await?;
        let result = reader.read(request).await;
        reader.close();
        result
    }

    pub async fn submit(&self, action: Action) -> Result<OperationSnapshot> {
        let writer = self.writer().await?;
        let result = submit_with(&writer, action).await;
        writer.close();
        result
    }

    pub async fn send_input(
        &self,
        conversation: &str,
        mut input: InputRequest,
        files: &[PathBuf],
    ) -> Result<OperationSnapshot> {
        let writer = self.writer().await?;
        let result: Result<OperationSnapshot> = async {
            for path in files {
                input.attachments.push(stage_file(&writer, path).await?);
            }
            input
                .attachments
                .sort_by(|left, right| left.hash.cmp(&right.hash));
            input
                .attachments
                .dedup_by(|left, right| left.hash == right.hash);
            submit_with(
                &writer,
                Action::Input {
                    conversation_id: conversation.to_owned(),
                    input,
                },
            )
            .await
        }
        .await;
        writer.close();
        result
    }

    pub fn print<T: Serialize>(&self, value: &T) -> Result<()> {
        print_value(value, self.json)
    }
}

pub async fn stage_file(writer: &Client, path: &Path) -> Result<vibeface_client::wire::Attachment> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("cannot read attachment {}", path.display()))?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_ATTACHMENT,
        "attachment must be a file of at most 64 MiB: {}",
        path.display()
    );
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("attachment needs a UTF-8 file name")?
        .to_owned();
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_ATTACHMENT + 1)
        .read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= MAX_ATTACHMENT,
        "attachment grew beyond 64 MiB: {}",
        path.display()
    );
    writer.store_attachment(name, data).await
}

pub(crate) async fn submit_with(client: &Client, action: Action) -> Result<OperationSnapshot> {
    let saved = client.stage(action).await?;
    let result = match saved.state {
        OperationState::Prepared => client.send_staged(&saved.id).await,
        OperationState::Unknown => client.reconcile(&saved.id).await,
        _ => return Ok(saved),
    };
    match result {
        Ok(snapshot) => Ok(snapshot),
        Err(error) => {
            let pending = client.pending().await?;
            let original = pending
                .into_iter()
                .find(|operation| operation.id == saved.id);
            if let Some(original) = original {
                eprintln!(
                    "Operation {} remains {:?}: {error:#}. Query this operation before retrying.",
                    original.id, original.state
                );
                Ok(original)
            } else {
                Err(error).with_context(|| {
                    format!(
                        "operation {} was saved; query its original result before retrying",
                        saved.id
                    )
                })
            }
        }
    }
}

pub fn require_accepted(operation: &OperationSnapshot) -> Result<()> {
    ensure!(
        operation.state == OperationState::Accepted,
        "operation {} is {:?}; inspect it with `vibeface client reconcile {}`",
        operation.id,
        operation.state,
        operation.id
    );
    Ok(())
}

pub fn print_value<T: Serialize>(value: &T, compact: bool) -> Result<()> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    if compact {
        serde_json::to_writer(&mut output, value)?;
    } else {
        serde_json::to_writer_pretty(&mut output, value)?;
    }
    writeln!(output)?;
    Ok(())
}

pub fn operation_event(operation: &OperationSnapshot) -> Value {
    json!({"type":"operation","operation":operation})
}

pub fn event_payload(event: &Value) -> &Value {
    if event["type"] == "changed" {
        event
            .get("payload")
            .filter(|payload| payload.is_object())
            .unwrap_or(event)
    } else {
        event
    }
}
