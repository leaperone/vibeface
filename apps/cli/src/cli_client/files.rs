use super::session::Profile;
use anyhow::{Context, Result, ensure};
use base64::Engine;
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use vibeface_bridge::platform::privatefs;
use vibeface_client::{Action, DeliveryAction, DeliveryPrepare, DeliveryRead, ReadRequest};

#[derive(Args)]
pub struct FilesOptions {
    #[command(subcommand)]
    pub command: FileCommand,
}

#[derive(Subcommand)]
pub enum FileCommand {
    /// Cache a file for typed input or workspace attachment references.
    Stage {
        #[arg(long)]
        file: PathBuf,
        /// Local draft owner; defaults to cli-file:<hash>.
        #[arg(long)]
        owner: Option<String>,
    },
    /// Release a local draft owner; pending operations keep their required files.
    Release {
        #[arg(long)]
        owner: String,
    },
    /// Download a conversation attachment and verify its SHA-256 before saving.
    Get {
        #[arg(long)]
        conversation: String,
        #[arg(long)]
        hash: String,
        #[arg(long)]
        output: PathBuf,
        /// Replace an existing output file after complete verification.
        #[arg(long)]
        force: bool,
    },
    /// Prepare experimental delivery using a local file and typed delivery terms.
    PrepareDelivery {
        #[arg(long)]
        file: PathBuf,
        /// DeliveryPrepare fields without content_base64; keep the original intent_id on retries.
        #[arg(long)]
        input: String,
    },
    /// Save a received delivery file after checking its order, size and SHA-256.
    SaveDelivery {
        #[arg(long)]
        order_id: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        force: bool,
    },
}

pub async fn run(profile: &Profile, options: FilesOptions) -> Result<()> {
    match options.command {
        FileCommand::Stage { file, owner } => {
            let writer = profile.writer().await?;
            let result: Result<Value> = async {
                let attachment = super::session::stage_file(&writer, &file).await?;
                let owner = owner.unwrap_or_else(|| format!("cli-file:{}", attachment.hash));
                writer
                    .retain_attachments(&owner, vec![attachment.clone()])
                    .await?;
                Ok(json!({"attachment":attachment,"owner":owner}))
            }
            .await;
            writer.close();
            profile.print(&result?)
        }
        FileCommand::Release { owner } => {
            let writer = profile.writer().await?;
            let result: Result<()> = async {
                writer.release_attachments(&owner).await?;
                writer.collect_attachments(vec![]).await
            }
            .await;
            writer.close();
            result?;
            profile.print(&json!({"owner":owner,"released":true}))
        }
        FileCommand::Get {
            conversation,
            hash,
            output,
            force,
        } => {
            let result = download(profile, &conversation, &hash, &output, force).await?;
            profile.print(&result)
        }
        FileCommand::PrepareDelivery { file, input } => {
            let mut document = crate::client_commands::json_document(&input)?;
            let fields = document
                .as_object_mut()
                .context("delivery terms must be a JSON object")?;
            ensure!(
                !fields.contains_key("content_base64"),
                "omit content_base64 when using --file"
            );
            let metadata = std::fs::metadata(&file)?;
            let status = profile
                .read(ReadRequest::Delivery(DeliveryRead::Status))
                .await?;
            let maximum = status["max_file_bytes"]
                .as_u64()
                .context("delivery size limit missing")?;
            ensure!(
                metadata.is_file() && metadata.len() <= maximum,
                "file exceeds this Node's delivery limit"
            );
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(&file)?
                .take(maximum.saturating_add(1))
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= maximum,
                "file grew beyond the delivery limit"
            );
            fields.entry("name").or_insert(json!(
                file.file_name()
                    .and_then(|name| name.to_str())
                    .context("file needs a UTF-8 name")?
            ));
            fields.insert(
                "content_base64".into(),
                json!(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            );
            let prepare: DeliveryPrepare = serde_json::from_value(document).map_err(|_| {
                anyhow::anyhow!(
                    "invalid delivery terms; supply DeliveryPrepare fields and a stable intent_id"
                )
            })?;
            let operation = profile
                .submit(Action::Delivery(DeliveryAction::Prepare(prepare)))
                .await?;
            profile.print(&json!({"operation":operation}))?;
            super::session::require_accepted(&operation)
        }
        FileCommand::SaveDelivery {
            order_id,
            output,
            force,
        } => {
            let order = profile
                .read(ReadRequest::Delivery(DeliveryRead::Get {
                    order_id: order_id.clone(),
                }))
                .await?;
            let file = profile
                .read(ReadRequest::Delivery(DeliveryRead::Read { order_id }))
                .await?;
            let expected = file["sha256"].as_str().context("delivery digest missing")?;
            let size = file["bytes"].as_u64().context("delivery size missing")?;
            ensure!(
                order["bytes"].as_u64() == Some(size)
                    && order["sha256"].as_str().is_none_or(|hash| hash == expected)
                    && order["name"]
                        .as_str()
                        .is_none_or(|name| file["name"].as_str() == Some(name)),
                "delivery file differs from its order"
            );
            let encoded = file["content_base64"]
                .as_str()
                .context("delivery content missing")?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .context("invalid delivery encoding")?;
            ensure!(
                bytes.len() as u64 == size && hex::encode(Sha256::digest(&bytes)) == expected,
                "delivery size or SHA-256 verification failed"
            );
            save_bytes(&output, &bytes, force)?;
            profile.print(&json!({"path":output,"hash":expected,"size":size,"verified":true}))
        }
    }
}

fn save_bytes(output: &Path, bytes: &[u8], force: bool) -> Result<()> {
    ensure!(
        force || !output.exists(),
        "output already exists; use --force to replace it"
    );
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temporary = parent.join(format!(
        ".vibeface-download-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let mut file = privatefs::create_new(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if force {
            privatefs::durable_rename(&temporary, output)?;
        } else {
            std::fs::hard_link(&temporary, output)?;
            std::fs::remove_file(&temporary)?;
            privatefs::sync_dir(parent)?;
        }
        Ok(())
    })();
    if temporary.exists() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub async fn download(
    profile: &Profile,
    conversation: &str,
    hash: &str,
    output: &Path,
    force: bool,
) -> Result<Value> {
    ensure!(
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "hash must be a lowercase SHA-256 value"
    );
    ensure!(
        force || !output.exists(),
        "output exists: {}; use --force to replace it",
        output.display()
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(
        parent.is_dir(),
        "output directory does not exist: {}",
        parent.display()
    );
    let temporary = parent.join(format!(
        ".vibeface-download-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let result: Result<Value> = async {
        let reader = profile.reader().await?;
        let mut file = privatefs::create_new(&temporary)?;
        let mut digest = Sha256::new();
        let mut offset = 0_u64;
        let mut expected_size = None;
        loop {
            let page = reader
                .read(ReadRequest::ArtifactBytes {
                    conversation_id: conversation.to_owned(),
                    blob_hash: hash.to_owned(),
                    offset,
                })
                .await?;
            ensure!(
                page["blob_hash"].as_str() == Some(hash) && page["offset"].as_u64() == Some(offset),
                "attachment response does not match requested file or offset"
            );
            let size = page["size"].as_u64().context("attachment size missing")?;
            ensure!(size <= 64 * 1024 * 1024, "attachment exceeds 64 MiB");
            ensure!(
                expected_size.is_none_or(|previous| previous == size),
                "attachment size changed during download"
            );
            expected_size = Some(size);
            let encoded = page["data"].as_str().context("attachment bytes missing")?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .context("invalid attachment encoding")?;
            ensure!(
                bytes.len() <= 512 * 1024 && offset + bytes.len() as u64 <= size,
                "invalid attachment page size"
            );
            file.write_all(&bytes)?;
            digest.update(&bytes);
            offset += bytes.len() as u64;
            match page["next_offset"].as_u64() {
                Some(next) => ensure!(
                    next == offset && next < size && !bytes.is_empty(),
                    "attachment download did not advance"
                ),
                None => {
                    ensure!(offset == size, "attachment download is incomplete");
                    break;
                }
            }
        }
        reader.close();
        ensure!(
            hex::encode(digest.finalize()) == hash,
            "attachment SHA-256 verification failed"
        );
        file.sync_all()?;
        drop(file);
        if force {
            privatefs::durable_rename(&temporary, output)?;
        } else {
            std::fs::hard_link(&temporary, output)
                .context("cannot publish verified attachment; output may already exist")?;
            std::fs::remove_file(&temporary)?;
            privatefs::sync_dir(parent)?;
        }
        Ok(json!({"path":output,"hash":hash,"size":offset,"verified":true}))
    }
    .await;
    if temporary.exists() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
