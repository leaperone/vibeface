//! Blob grants follow effective signed inputs, including their successful edit-control chain.
use super::{Store, membership_from};
use crate::{model::*, workspace::{InputControl, QueuedInput, WORKSPACE_SCOPE, WorkspaceCommand}};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{OptionalExtension as _, params};
use std::collections::BTreeSet;

pub(super) struct InputAttachmentSource {
    pub input: QueuedInput,
    pub original: HashedEvent,
    pub source: HashedEvent,
}

impl Store {
    fn effective_attachment_event(&self, network: &str, hash: &str) -> Result<HashedEvent> {
        let encoded: String = self.connection.query_row(
            "SELECT envelope FROM events WHERE network_id=? AND hash=? AND effective=1",
            params![network, hash], |row| row.get(0),
        ).optional()?.context("attachment source is not effective in this personal network")?;
        let signed: SignedEvent = serde_json::from_str(&encoded)?;
        ensure!(signed.hash == hash, "attachment source hash mismatch");
        Ok(HashedEvent { hash: hash.into(), event: serde_json::from_str(&signed.canonical_body)? })
    }

    fn human_attachment_event(&self, network: &str, source: &HashedEvent) -> Result<()> {
        let event = &source.event;
        ensure!(event.network_id == network, "wrong personal network");
        let attestation = event.body.attestation().context("attachment input needs the user's signature")?;
        ensure!(membership_from(&self.connection, network)?.attester(&attestation.device_id),
            "attachment input is not signed by a client of this personal network");
        crate::crypto::verify_attestation(network, &event.scope_id, &event.body)
    }

    /// Resolve the edit that actually supplies the current projected attachments. Move and
    /// Release retain that grant through their signed compare-and-swap predecessor chain.
    pub(super) fn input_attachment_source(&self, network: &str, message_id: &str) -> Result<Option<InputAttachmentSource>> {
        let Some(input) = self.queued_workspace_input(network, message_id)? else { return Ok(None); };
        if input.input.attachments.is_empty() { return Ok(None); }
        let original = self.effective_attachment_event(network, &input.source_event)?;
        self.human_attachment_event(network, &original)?;
        ensure!(original.event.scope_id == input.conversation_id
            && matches!(&original.event.body, EventBody::InputPosted { message_id: id, .. } if id == message_id),
            "attachment input belongs to another conversation");
        let mut next = input.control_event.clone();
        let mut visited = BTreeSet::new();
        let mut edit = None;
        let source = loop {
            ensure!(visited.insert(next.clone()), "attachment control chain is cyclic");
            if next == original.hash { break edit.unwrap_or_else(|| original.clone()); }
            let control = self.effective_attachment_event(network, &next)?;
            self.human_attachment_event(network, &control)?;
            ensure!(control.event.scope_id == WORKSPACE_SCOPE, "attachment edit belongs to another scope");
            let EventBody::WorkspaceCommanded {
                command: WorkspaceCommand::ControlInput { message_id: id, expected_event, action },
                cause: None, acting_assignment: None, ..
            } = &control.event.body else { anyhow::bail!("attachment control source is not a human input control"); };
            ensure!(id == message_id && control.event.prev.iter().chain(&control.event.deps).any(|hash| hash == expected_event),
                "attachment control is outside the signed input chain");
            if edit.is_none() && matches!(action, InputControl::Edit { .. }) { edit = Some(control.clone()); }
            next = expected_event.clone();
        };
        let attachments = match &source.event.body {
            EventBody::InputPosted { attachments, .. }
            | EventBody::WorkspaceCommanded { command: WorkspaceCommand::ControlInput {
                action: InputControl::Edit { attachments, .. }, .. }, .. } => attachments,
            _ => anyhow::bail!("attachment source does not carry files"),
        };
        ensure!(attachments == &input.input.attachments, "attachment projection differs from its signed source");
        for attachment in attachments {
            ensure!(source.event.blobs.contains(&attachment.blob()), "attachment is outside its signed blob references");
        }
        Ok(Some(InputAttachmentSource { input, original, source }))
    }

    pub(super) fn edited_attachment_sources(&self, network: &str, conversation: &str, hash: &str) -> Result<Vec<InputAttachmentSource>> {
        let has_edit: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM events e JOIN event_blobs b ON b.network_id=e.network_id AND b.hash=e.hash
             WHERE e.network_id=? AND e.scope_id=? AND e.kind='workspace_commanded' AND e.effective=1 AND b.blob=?)",
            params![network, WORKSPACE_SCOPE, hash], |row| row.get(0),
        )?;
        if !has_edit { return Ok(vec![]); }
        let mut sources = Vec::new();
        for input in self.workspace_queue(network)? {
            if input.conversation_id == conversation && input.input.attachments.iter().any(|attachment| attachment.hash == hash)
                && let Some(source) = self.input_attachment_source(network, &input.input.message_id)?
            {
                sources.push(source);
            }
        }
        Ok(sources)
    }

    pub(super) fn attachment_reference_size(&self, network: &str, scope: &str, hash: &str) -> Result<u64> {
        let direct: Option<u64> = self.connection.query_row(
            "SELECT b.size FROM event_blobs b JOIN events e ON e.network_id=b.network_id AND e.hash=b.hash
             WHERE b.network_id=? AND b.blob=? AND e.scope_id=? AND e.effective=1 LIMIT 1",
            params![network, hash, scope], |row| row.get(0),
        ).optional()?;
        if let Some(size) = direct { return Ok(size); }
        if scope != WORKSPACE_SCOPE {
            for source in self.edited_attachment_sources(network, scope, hash)? {
                if let Some(attachment) = source.input.input.attachments.iter().find(|attachment| attachment.hash == hash) {
                    return Ok(attachment.size);
                }
            }
        }
        anyhow::bail!("attachment is not referenced by this conversation")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{crypto::LocalIdentity, workspace::{InputIntent, InputRequest, InputRoute, InputTarget}};

    struct PrivateDirectory(std::path::PathBuf);
    impl Drop for PrivateDirectory {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }
    struct Fixture { node: LocalIdentity, admin: LocalIdentity, network: String, state: crate::identity::NetworkState,
        store: Store, _directory: PrivateDirectory }

    fn fixture() -> Result<Fixture> {
        let node = LocalIdentity::generate(); let admin = node.fixture_admin();
        let state = node.sign_network_fixture(1, vec![node.device_id()?])?;
        let network = node.fixture_network_id()?;
        let directory = std::env::temp_dir().join(format!("vibeface-attachment-scope-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&directory)?;
        let mut store = Store::open(directory.join("node.sqlite3"))?;
        store.install_trust_bundle(&network, &state)?;
        store.create_conversation(&node, &network, "c", "Attachment fixture", node.owner_fixture(&network, "c", &node.device_id()?)?, 1)?;
        let body = admin.attest(&network, "c", EventBody::BotCreated { bot_id: "bot".into(), name: "Attachment fixture".into(),
            instructions: "Use synthetic inputs".into(), project: None, attestation: None, cause: None })?;
        store.append_event(&node, &network, "c", body, vec![], 2)?;
        Ok(Fixture { node, admin, network, state, store, _directory: PrivateDirectory(directory) })
    }
    fn command(f: &mut Fixture, id: &str, command: WorkspaceCommand, at: i64) -> Result<crate::workspace::WorkspaceCommandReceipt> {
        let signed = f.admin.attest(&f.network, WORKSPACE_SCOPE, EventBody::WorkspaceCommanded { command_id: id.into(), command: command.clone(),
            attestation: None, cause: None, acting_assignment: None })?;
        f.store.apply_workspace_command(&f.node, &f.network, id, command, signed.attestation().cloned(), None, None, at)
    }
    fn post(f: &mut Fixture, id: &str, at: i64) -> Result<String> {
        let request = InputRequest { message_id: id.into(), route: InputRoute { target: InputTarget::BotMain { bot_id: "bot".into() },
            intent: InputIntent::Defer, sequence: at as u64, expected_revision: None }, text: "Saved input".into(), attachments: vec![], report_results: Some(true) };
        let signed = f.admin.attest(&f.network, "c", EventBody::InputPosted { message_id: id.into(), target: request.route.target.clone(),
            intent: request.route.intent, sequence: request.route.sequence, expected_revision: None, text: request.text.clone(),
            attachments: vec![], report_results: request.report_results, attestation: None })?;
        Ok(f.store.post_workspace_input(&f.node, &f.network, request, signed.attestation().cloned().context("input signature")?, at)?.source_event)
    }
    fn file(f: &mut Fixture, content: &[u8], name: &str) -> Result<Attachment> {
        let attachment = Attachment { hash: crate::crypto::digest(content), size: content.len() as u64, name: name.into() };
        f.store.put_blob(&attachment.blob(), content)?; Ok(attachment)
    }
    fn edit(f: &mut Fixture, source: &str, attachment: &Attachment) -> Result<String> {
        Ok(command(f, "edit", WorkspaceCommand::ControlInput { message_id: "input".into(), expected_event: source.into(),
            action: InputControl::Edit { text: "Edited input".into(), attachments: vec![attachment.clone()] } }, 4)?.event_hash)
    }
    fn original_event_seq(store: &Store, network: &str, hash: &str) -> Result<u64> {
        Ok(store.effective_attachment_event(network, hash)?.event.seq)
    }
    fn rebuild(f: &mut Fixture) -> Result<()> {
        f.store.connection.execute("UPDATE events SET effective=0 WHERE network_id=?", [&f.network])?;
        crate::store::reduce::clear(&f.store.connection, &f.network)?;
        crate::store::log::recompute(&f.store.connection, &membership_from(&f.store.connection, &f.network)?)?;
        Ok(())
    }

    #[test]
    fn edited_files_remain_in_the_original_conversation_after_move_release_and_rebuild() -> Result<()> {
        let mut f = fixture()?;
        let original = post(&mut f, "input", 3)?;
        let bytes = b"edited signed input file";
        let attachment = file(&mut f, bytes, "edited.txt")?;
        let edit = edit(&mut f, &original, &attachment)?;
        let moved = command(&mut f, "move", WorkspaceCommand::ControlInput { message_id: "input".into(), expected_event: edit.clone(),
            action: InputControl::Move { before_message_id: None } }, 5)?;
        command(&mut f, "release", WorkspaceCommand::ControlInput { message_id: "input".into(), expected_event: moved.event_hash,
            action: InputControl::Release }, 6)?;
        let claimed = f.store.claim_next_fixture(&f.node, &f.network, "c", "boot", "fixture")?.context("claimed edited input")?;
        let ledger = serde_json::to_value(f.store.scheduler_jobs_all()?)?;
        for _ in 0..2 {
            let source = f.store.input_attachment_source(&f.network, "input")?.context("source")?;
            assert_eq!(source.source.hash, edit);
            assert_eq!(source.original.hash, original);
            assert_eq!(f.store.read_blob(&f.network, "c", &attachment.hash)?.1, bytes);
            assert_eq!(f.store.read_blob_range(&f.network, "c", &attachment.hash, 7, 6)?.1, b"signed");
            assert_eq!(f.store.verified_input_attachment(&f.network, "c", "input", &attachment)?.1, bytes);
            assert_eq!(f.store.input_attachment_events(&f.network, "input")?.iter().map(|source| source.hash.as_str()).collect::<Vec<_>>(), vec![edit.as_str()]);
            assert_eq!(f.store.scheduler_blob_scope(&f.network, &claimed.job, &attachment.hash)?, Some("c".into()));
            assert_eq!(f.store.main_blob_scope(&f.network, "c", &attachment.hash)?, Some("c".into()));
            let page = crate::runtime::agent_tools::blob_page(&f.store, &f.network, "c", &attachment.hash, &serde_json::json!({}))?;
            assert_eq!(page["text"], "edited signed input file");
            let mut wrong_job = claimed.job.clone(); wrong_job.conversation_id = "other".into();
            assert_eq!(f.store.scheduler_blob_scope(&f.network, &wrong_job, &attachment.hash)?, None);
            let mut before_edit = claimed.job.clone(); before_edit.basis.insert(f.node.device_id()?, original_event_seq(&f.store, &f.network, &original)?);
            assert_eq!(f.store.scheduler_blob_scope(&f.network, &before_edit, &attachment.hash)?, None);
            assert!(f.store.read_blob(&f.network, "other", &attachment.hash).is_err());
            assert!(f.store.read_blob("another-network", "c", &attachment.hash).is_err());
            assert!(f.store.read_blob(&f.network, "c", &crate::crypto::digest(b"missing")).is_err());
            assert!(f.store.verified_input_attachment(&f.network, "other", "input", &attachment).is_err());
            let mut wrong_metadata = attachment.clone(); wrong_metadata.size += 1;
            assert!(f.store.verified_input_attachment(&f.network, "c", "input", &wrong_metadata).is_err());
            rebuild(&mut f)?;
            assert_eq!(serde_json::to_value(f.store.scheduler_jobs_all()?)?, ledger);
        }
        Ok(())
    }

    #[test]
    fn channel_files_do_not_grant_the_coordinators_conversation_access() -> Result<()> {
        let mut f = fixture()?;
        let bytes = b"channel-only file"; let attachment = file(&mut f, bytes, "channel.txt")?;
        let local = f.node.device_id()?;
        command(&mut f, "channel", WorkspaceCommand::CreateChannel { channel_id: "channel".into(), title: "Synthetic channel".into(),
            purpose: "Attachment isolation".into(), members: vec!["bot".into()], coordinator_bot: "bot".into(), coordinator_node: local }, 3)?;
        command(&mut f, "channel-file", WorkspaceCommand::PostChannelMessage { channel_id: "channel".into(), message_id: "channel-message".into(),
            text: "File".into(), attachments: vec![attachment.clone()], recipient_bot: None, task_id: None, reply_to: None }, 4)?;
        assert_eq!(f.store.read_blob(&f.network, WORKSPACE_SCOPE, &attachment.hash)?.1, bytes);
        assert!(f.store.read_blob(&f.network, "c", &attachment.hash).is_err());
        let original = post(&mut f, "input", 5)?;
        let signed = f.store.input_attachment_source(&f.network, "input")?;
        assert!(signed.is_none());
        let forged = f.admin.attest(&f.network, WORKSPACE_SCOPE, EventBody::WorkspaceCommanded {
            command_id: "forged-edit".into(), command: WorkspaceCommand::ControlInput { message_id: "input".into(), expected_event: original,
                action: InputControl::Edit { text: "Forged".into(), attachments: vec![attachment.clone()] } }, attestation: None, cause: None, acting_assignment: None })?;
        let EventBody::WorkspaceCommanded { mut command, .. } = forged.clone() else { unreachable!() };
        if let WorkspaceCommand::ControlInput { action: InputControl::Edit { text, .. }, .. } = &mut command {
            *text = "Changed after signing".into();
        }
        assert!(f.store.apply_workspace_command(&f.node, &f.network, "forged-edit", command,
            forged.attestation().cloned(), None, None, 6).is_err());
        assert!(f.store.verified_input_attachment(&f.network, "c", "input", &attachment).is_err());
        assert!(f.store.read_blob_range(&f.network, "c", &attachment.hash, 0, 4).is_err());
        assert_eq!(f.store.main_blob_scope(&f.network, "c", &attachment.hash)?, None);
        assert!(f.store.read_blob("another-network", WORKSPACE_SCOPE, &attachment.hash).is_err());
        rebuild(&mut f)?;
        assert_eq!(f.store.read_blob(&f.network, WORKSPACE_SCOPE, &attachment.hash)?.1, bytes);
        assert!(f.store.read_blob(&f.network, "c", &attachment.hash).is_err());
        Ok(())
    }

    #[test]
    fn rejected_edits_and_governance_rollback_cannot_leave_attachment_grants() -> Result<()> {
        let mut f = fixture()?;
        let original = post(&mut f, "input", 3)?;
        let original_event = f.store.effective_attachment_event(&f.network, &original)?;
        let attachment = file(&mut f, b"authorized edit", "edit.txt")?;
        let edit_hash = edit(&mut f, &original, &attachment)?;
        let unrelated = file(&mut f, b"uncommitted edit", "rejected.txt")?;
        let before = f.store.version_vector(&f.network)?;
        assert!(command(&mut f, "rejected", WorkspaceCommand::ControlInput { message_id: "input".into(), expected_event: original.clone(),
            action: InputControl::Edit { text: "Wrong CAS".into(), attachments: vec![unrelated.clone()] } }, 5).is_err());
        assert_eq!(f.store.version_vector(&f.network)?, before);
        assert!(f.store.read_blob(&f.network, "c", &unrelated.hash).is_err());
        assert!(f.store.read_blob(&f.network, WORKSPACE_SCOPE, &unrelated.hash).is_err());
        assert_eq!(f.store.input_attachment_source(&f.network, "input")?.context("edit survives failed CAS")?.source.hash, edit_hash);
        let mut revoked = f.state.clone();
        revoked.append(crate::identity::GovernanceEvent::revoke(&f.admin, &f.state.project()?,
            crate::identity::Revocation::new(&f.node.device_id()?, Some((original_event.event.seq, &original)))?)?)?;
        f.store.install_trust_bundle(&f.network, &revoked)?;
        assert!(f.store.read_blob(&f.network, "c", &attachment.hash).is_err());
        assert!(f.store.read_blob(&f.network, WORKSPACE_SCOPE, &attachment.hash).is_err());
        assert!(f.store.input_attachment_events(&f.network, "input")?.is_empty());
        rebuild(&mut f)?;
        assert!(f.store.read_blob(&f.network, "c", &attachment.hash).is_err());
        Ok(())
    }
}
