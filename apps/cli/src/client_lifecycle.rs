//! Local Node joins use the owner socket; enrollment withdrawal proves this client's own key.

use crate::manage::{ClientAccess, operator};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde_json::{Value, json};
use std::io::Read;
use vibeface_client::Enrollment;
use zeroize::Zeroizing;

const MAX_INVITE_BYTES: usize = 4096;

#[derive(Subcommand)]
pub enum LifecycleCommand {
    /// Read this local Node's terminal-started join without starting or resuming it.
    NodeJoinStatus,
    /// Check an invite from this local, uninitialized Node before joining it.
    NodeJoinCheck {
        /// Invite link; `-` reads it from standard input. The link is never printed.
        link: String,
    },
    /// Request cancellation of this local Node's terminal-started join.
    ///
    /// The reply reports its current status. Use node-join-status to observe completion;
    /// an approval that completed first remains joined. A webpage's join keeps its owner.
    NodeJoinCancel,
    /// Withdraw this client's saved enrollment using its original join ID and identity.
    ///
    /// No new join is submitted. If approval completed first, the reply reports joined;
    /// resume the original enrollment to verify and save membership.
    WithdrawEnrollment,
}

pub async fn run(access: &ClientAccess, command: LifecycleCommand) -> Result<()> {
    if !matches!(command, LifecycleCommand::WithdrawEnrollment) {
        ensure!(
            access.node.is_none() && access.url.is_none(),
            "local Node join commands use --data-dir, not --node or --url"
        );
    }
    let result = match command {
        LifecycleCommand::NodeJoinStatus => {
            operator(&access.data_dir, "node_join_status", json!({})).await?
        }
        LifecycleCommand::NodeJoinCheck { link } => {
            let link = read_invite(link)?;
            operator(&access.data_dir, "node_join_check", check_params(&link)?).await?
        }
        LifecycleCommand::NodeJoinCancel => {
            operator(&access.data_dir, "node_join_cancel", json!({})).await?
        }
        LifecycleCommand::WithdrawEnrollment => {
            let key = access.unlock()?;
            ensure!(
                Enrollment::pending(&key),
                "no saved enrollment; there is no original join ID to withdraw"
            );
            Enrollment::withdraw_pending(&key).await?
        }
    };
    crate::cli_client::session::print_value(&result, access.json)
}

fn read_invite(link: String) -> Result<Zeroizing<String>> {
    let mut link = Zeroizing::new(link);
    if link.as_str() == "-" {
        link.clear();
        std::io::stdin()
            .take(MAX_INVITE_BYTES as u64 + 1)
            .read_to_string(&mut link)
            .context("cannot read the invite from standard input")?;
    }
    Ok(link)
}

fn check_params(link: &str) -> Result<Value> {
    ensure!(link.len() <= MAX_INVITE_BYTES, "invite exceeds 4096 bytes");
    let link = link.trim();
    ensure!(
        !link.is_empty() && !link.chars().any(char::is_control),
        "invite must be a nonempty link on one line"
    );
    Ok(json!({"link":link}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Options {
        #[command(subcommand)]
        command: LifecycleCommand,
    }

    #[test]
    fn named_lifecycle_commands_parse_without_membership_options() -> Result<()> {
        assert!(matches!(
            Options::try_parse_from(["client", "node-join-status"])?.command,
            LifecycleCommand::NodeJoinStatus
        ));
        assert!(matches!(
            Options::try_parse_from(["client", "node-join-cancel"])?.command,
            LifecycleCommand::NodeJoinCancel
        ));
        assert!(matches!(
            Options::try_parse_from(["client", "withdraw-enrollment"])?.command,
            LifecycleCommand::WithdrawEnrollment
        ));
        let parsed = Options::try_parse_from(["client", "node-join-check", "-"])?;
        let LifecycleCommand::NodeJoinCheck { link } = parsed.command else {
            anyhow::bail!("expected the invite-check command")
        };
        assert_eq!(link, "-");
        assert!(Options::try_parse_from(["client", "node-join-check"]).is_err());
        assert!(Options::try_parse_from(["client", "withdraw-enrollment", "another-id"]).is_err());
        Ok(())
    }

    #[test]
    fn invite_check_sends_only_the_link_to_the_owner_socket() -> Result<()> {
        assert_eq!(
            check_params("  vibeface1fixture\n")?,
            json!({"link":"vibeface1fixture"})
        );
        for invalid in [
            "".to_owned(),
            "\n".into(),
            "fixture\nsecond-line".into(),
            "x".repeat(4097),
        ] {
            assert!(check_params(&invalid).is_err());
        }
        let secret = "private-invite\0contents";
        assert!(
            !check_params(secret)
                .unwrap_err()
                .to_string()
                .contains("private-invite")
        );
        Ok(())
    }
}
