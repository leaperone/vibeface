use super::session::print_value;
use crate::manage::ClientAccess;
use anyhow::{Result, bail, ensure};
use clap::{Args, Subcommand};
use serde_json::json;
use vibeface_client::{IdentityOperationState, IdentityPlan};

#[derive(Args)]
pub struct IdentityOptions {
    /// Password for the recovery file belonging to this saved identity operation.
    #[arg(long, global = true)]
    recovery_password_fd: Option<i64>,
    #[command(subcommand)]
    command: IdentityCommand,
}

#[derive(Subcommand)]
enum IdentityCommand {
    /// Inspect saved public metadata without repairing files or contacting a Node.
    Status,
    /// Continue the exact saved operation, or query its receipt if already sent.
    Resume,
    /// Remove a never-sent or conclusively settled journal; keep the recovery backup.
    Discard,
}

pub async fn run(access: &ClientAccess, options: IdentityOptions) -> Result<()> {
    ensure!(
        access.url.is_none() && access.node.is_none(),
        "saved identity operations retain their original target; omit --url and --node"
    );
    let mut key = access.unlock()?;
    if !IdentityPlan::exists(&key) {
        if matches!(options.command, IdentityCommand::Status) {
            return print_value(&json!({"plan":null}), access.json);
        }
        bail!("there is no saved identity operation");
    }
    let password = crate::read_passphrase(
        options.recovery_password_fd,
        "saved recovery password",
        false,
    )?;
    match options.command {
        IdentityCommand::Status => print_value(
            &json!({"plan":IdentityPlan::inspect(&key, password.as_str())?}),
            access.json,
        ),
        IdentityCommand::Discard => {
            IdentityPlan::discard_saved(&key, password.as_str())?;
            print_value(
                &json!({"discarded":true,"backup_preserved":true}),
                access.json,
            )
        }
        IdentityCommand::Resume => {
            let mut plan = IdentityPlan::load(&key, password.as_str())?;
            let result = plan.submit(&mut key).await;
            let state = plan.state();
            let info = IdentityPlan::inspect(&key, password.as_str())?;
            if result.is_ok() && state == IdentityOperationState::Applied {
                plan.discard()?;
            }
            print_value(
                &json!({"plan":info,"state":state,"result":result.as_ref().ok()}),
                access.json,
            )?;
            result?;
            ensure!(
                state != IdentityOperationState::Stopped,
                "the saved recovery was stopped; no new request was sent"
            );
            Ok(())
        }
    }
}
