pub mod chat;
mod files;
mod identity;
pub mod session;

use crate::{
    client_commands::{BusinessCommand, Request},
    manage::ClientAccess,
};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde_json::{Value, json};
use session::{Profile, print_value, require_accepted};
use sha2::{Digest, Sha256};
use std::{io::Write, path::PathBuf, time::Duration};
use vibeface_bridge::platform::privatefs;
use vibeface_client::{Action, ClientReader, ReadRequest};

#[derive(Subcommand)]
pub enum ClientCommand {
    /// Subscribe to changes and refresh snapshots after a reconnect.
    Watch {
        #[arg(long)]
        conversation: Option<String>,
    },
    /// List durable operations whose outcome still needs attention.
    Pending,
    /// Read back the original operation and resume only a prepared request.
    Reconcile { operation_id: String },
    /// Remove a conclusively rejected local operation from the pending list.
    DismissRejected { operation_id: String },
    /// Show the redacted settings intent that needs an explicit review.
    SettingsReview { operation_id: String },
    /// Close a reviewed unknown settings intent without sending it again.
    SettingsClose {
        operation_id: String,
        /// Hash printed by settings-review after checking the actual Node settings.
        #[arg(long)]
        review_hash: String,
        #[arg(long)]
        note: String,
    },
    /// Read and save conversation files.
    Files(files::FilesOptions),
    /// List supported typed operations, examples and Web API mappings.
    Capabilities,
    /// Inspect, resume or explicitly discard the saved identity operation.
    IdentityPlan(identity::IdentityOptions),
    /// Save the selected Gateway and target for this client identity.
    RememberEntry,
    /// Replace the recovery file and password while keeping all current members.
    RotateRecovery {
        #[arg(long)]
        recovery_file: PathBuf,
        #[arg(long)]
        new_recovery_file: PathBuf,
        #[arg(long)]
        recovery_password_fd: Option<i64>,
        #[arg(long)]
        new_recovery_password_fd: Option<i64>,
    },
}

pub async fn business(access: &ClientAccess, command: BusinessCommand) -> Result<()> {
    let request = command.into_request()?;
    let profile = Profile::open(access)?;
    match request {
        Request::Read(request) => profile.print(&profile.read(request).await?),
        Request::Write(action) => {
            if matches!(action, Action::CreateDecisionToken { .. }) {
                let path = access
                    .secret_output
                    .as_ref()
                    .context("creating a decision token requires --secret-output FILE")?;
                let writer = profile.writer().await?;
                let mut file = privatefs::create_new(path)
                    .context("cannot create private token output file")?;
                let result: Result<()> = async {
                    let operation = session::submit_with(&writer, action).await?;
                    if operation.state != vibeface_client::OperationState::Accepted {
                        profile.print(&json!({"operation":operation}))?;
                    }
                    require_accepted(&operation)?;
                    let token = writer.take_decision_token(&operation.id)?.context("the one-time token was not returned; inspect the operation before creating another")?;
                    serde_json::to_writer(&mut file, &json!({"token_id":token.token_id,"token":token.expose()}))?;
                    writeln!(file)?;
                    file.sync_all()?;
                    profile.print(&json!({"operation":operation,"secret_output":path,"secret_saved":true}))?;
                    Ok(())
                }.await;
                writer.close();
                if result.is_err() && file.metadata()?.len() == 0 {
                    let _ = std::fs::remove_file(path);
                }
                result
            } else {
                let operation = profile.submit(action).await?;
                profile.print(&json!({"operation":operation}))?;
                require_accepted(&operation)
            }
        }
    }
}

pub async fn run(access: &ClientAccess, command: ClientCommand) -> Result<()> {
    if let ClientCommand::IdentityPlan(options) = command {
        return identity::run(access, options).await;
    }
    if matches!(command, ClientCommand::Capabilities) {
        return print_value(&crate::client_commands::capabilities(), access.json);
    }
    if matches!(command, ClientCommand::RememberEntry) {
        let url = access
            .url
            .clone()
            .context("remember-entry requires --url")?;
        let node = access
            .node
            .clone()
            .context("remember-entry requires --node")?;
        let mut key = access.unlock()?;
        let entry = vibeface_client::Entry {
            origin: url,
            node_id: node,
        };
        vibeface_client::Endpoint::Gateway(entry.clone()).validate()?;
        let network = key
            .network_id()
            .context("client is not enrolled")?
            .to_owned();
        let mut session = key.connect(Some(&entry)).await?;
        let directory = session.directory().await?;
        directory.view.node(&entry.node_id)?;
        directory.view.client(key.device_id())?;
        session.connection.close();
        key.remember(&network, entry.clone())?;
        return print_value(&json!({"entry":entry}), access.json);
    }
    if let ClientCommand::RotateRecovery {
        recovery_file,
        new_recovery_file,
        recovery_password_fd,
        new_recovery_password_fd,
    } = command
    {
        let mut key = access.unlock()?;
        let entry = access
            .entry(&key)?
            .or_else(|| key.entry().cloned())
            .context("recovery rotation needs a remembered Gateway or --url")?;
        let password =
            crate::read_passphrase(recovery_password_fd, "current recovery password", false)?;
        let new_password =
            crate::read_passphrase(new_recovery_password_fd, "new recovery password", true)?;
        if vibeface_client::IdentityPlan::exists(&key) {
            let info = match vibeface_client::IdentityPlan::inspect(&key, new_password.as_str()) {
                Ok(info) => info,
                Err(_) => {
                    vibeface_client::IdentityPlan::discard_applied(&key, password.as_str())?;
                    vibeface_client::IdentityPlan::inspect(&key, new_password.as_str())?
                }
            };
            if info
                .as_ref()
                .is_some_and(|info| info.state == vibeface_client::IdentityOperationState::Applied)
            {
                vibeface_client::IdentityPlan::discard_applied(&key, new_password.as_str())?;
            }
        }
        let mut plan = if vibeface_client::IdentityPlan::exists(&key) {
            let info = vibeface_client::IdentityPlan::inspect(&key, new_password.as_str())?
                .context("pending identity operation missing")?;
            ensure!(
                info.kind == vibeface_client::IdentityPlanKind::KeyRotation && info.target == entry,
                "another identity operation is pending; inspect it with client identity-plan status"
            );
            let plan = vibeface_client::IdentityPlan::load(&key, new_password.as_str())?;
            ensure!(
                plan.backup_path() == new_recovery_file,
                "a different identity operation is pending; resume its original backup first"
            );
            plan
        } else {
            vibeface_client::prepare_key_rotation(
                &key,
                entry,
                &recovery_file,
                password.as_str(),
                &new_recovery_file,
                new_password.as_str(),
            )
            .await?
        };
        eprintln!(
            "Replacement recovery file saved and verified: {}",
            plan.backup_path().display()
        );
        let result = plan.submit(&mut key).await?;
        let backup_path = plan.backup_path().to_owned();
        let state = plan.state();
        if state == vibeface_client::IdentityOperationState::Applied {
            plan.discard()?;
        }
        return print_value(
            &json!({"result":result,"recovery_file":backup_path,"state":state}),
            access.json,
        );
    }
    let profile = Profile::open(access)?;
    match command {
        ClientCommand::Watch { conversation } => watch(&profile, conversation.as_deref()).await,
        ClientCommand::Files(options) => files::run(&profile, options).await,
        ClientCommand::Pending => {
            let writer = profile.writer().await?;
            let pending = writer.pending().await;
            writer.close();
            profile.print(&json!({"operations":pending?}))
        }
        ClientCommand::Reconcile { operation_id } => {
            let writer = profile.writer().await?;
            let operation = writer.reconcile(&operation_id).await;
            writer.close();
            let operation = operation?;
            profile.print(&json!({"operation":operation}))?;
            require_accepted(&operation)
        }
        ClientCommand::DismissRejected { operation_id } => {
            let writer = profile.writer().await?;
            let result = writer.dismiss_rejected(&operation_id).await;
            writer.close();
            result?;
            profile.print(&json!({"operation_id":operation_id,"dismissed":true}))
        }
        ClientCommand::SettingsReview { operation_id } => {
            let writer = profile.writer().await?;
            let review = writer.review_settings(&operation_id).await;
            writer.close();
            let review = review?;
            let value = redacted_review(&review);
            profile.print(&json!({"review":value,"review_hash":review_hash(&value)?}))
        }
        ClientCommand::SettingsClose {
            operation_id,
            review_hash: expected,
            note,
        } => {
            ensure!(!note.trim().is_empty(), "review note cannot be empty");
            let writer = profile.writer().await?;
            let result: Result<Value> = async {
                let review = writer.review_settings(&operation_id).await?;
                ensure!(
                    review_hash(&redacted_review(&review))? == expected,
                    "settings review changed; run settings-review again"
                );
                let operations = writer.close_unknown_settings(review, note).await?;
                Ok(json!({"operations":operations}))
            }
            .await;
            writer.close();
            profile.print(&result?)
        }
        ClientCommand::Capabilities
        | ClientCommand::IdentityPlan(_)
        | ClientCommand::RememberEntry
        | ClientCommand::RotateRecovery { .. } => unreachable!("handled before opening the client"),
    }
}

fn redacted_review(review: &vibeface_client::SettingsReview) -> Value {
    json!({"operation_id":review.operation_id,"network_id":review.network_id,
        "client_id":review.client_id,"target_node":review.target_node,"kind":review.kind,
        "fields":review.fields.iter().map(|field|json!({"name":field.name,
            "value":if field.secret { "[private change]" } else { &field.value },"secret":field.secret})).collect::<Vec<_>>(),
        "has_secret_changes":review.has_secret_changes,"dependent_ids":review.dependent_ids})
}

fn review_hash(value: &Value) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}

async fn snapshot(reader: &ClientReader, conversation: Option<&str>) -> Result<Value> {
    match conversation {
        Some(id) => {
            reader
                .read(ReadRequest::Conversation {
                    conversation_id: id.to_owned(),
                })
                .await
        }
        None => {
            let (home, nodes, works, questions) = tokio::try_join!(
                reader.read(ReadRequest::Home),
                reader.read(ReadRequest::Nodes),
                reader.read(ReadRequest::OpenWorks),
                reader.read(ReadRequest::Questions)
            )?;
            Ok(json!({"home":home,"nodes":nodes,"open_works":works,"questions":questions}))
        }
    }
}

async fn watch(profile: &Profile, conversation: Option<&str>) -> Result<()> {
    let mut reader = profile.reader().await?;
    loop {
        let mut events = reader.subscribe();
        print_value(
            &json!({"type":"snapshot","generation":reader.generation(),"value":snapshot(&reader, conversation).await?}),
            true,
        )?;
        loop {
            let event = tokio::select! {
                result = events.recv() => result,
                result = tokio::signal::ctrl_c() => { result?; reader.close(); return Ok(()); }
            };
            match event {
                Ok(event) => {
                    let payload = session::event_payload(&event.payload);
                    if payload["type"] == "disconnected" {
                        break;
                    }
                    if payload["type"] == "resync_required" {
                        break;
                    }
                    let scope = payload["conversation_id"]
                        .as_str()
                        .or_else(|| payload["scope_id"].as_str());
                    if conversation.is_none() || scope.is_none() || scope == conversation {
                        print_value(
                            &json!({"type":"event","generation":event.generation,"network_id":event.network_id,"payload":payload}),
                            true,
                        )?;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        print_value(
            &json!({"type":"resync_required","generation":reader.generation()}),
            true,
        )?;
        reader.close();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                result = tokio::signal::ctrl_c() => { result?; return Ok(()); }
            }
            match profile.reader().await {
                Ok(next) => {
                    reader = next;
                    break;
                }
                Err(error) => eprintln!("Waiting to reconnect: {error:#}"),
            }
        }
    }
}
