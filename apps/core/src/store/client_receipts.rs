//! Positive evidence for a native client's exact signed command. Absence never permits replay.
use super::*;
use anyhow::Context as _;
use crate::workspace::{WORKSPACE_SCOPE, WorkspaceCommand};
use serde_json::json;

impl Store {
    pub fn client_workspace_receipt(
        &self,
        network: &str,
        client_id: &str,
        command_id: &str,
        command: &WorkspaceCommand,
        attestation: &Attestation,
    ) -> Result<Value> {
        vibeface_protocol::validate_id(command_id).map_err(anyhow::Error::msg)?;
        ensure!(attestation.device_id == client_id, "receipt belongs to another client");
        let body = EventBody::WorkspaceCommanded {
            command_id: command_id.into(), command: command.clone(),
            acting_assignment: None, attestation: Some(attestation.clone()), cause: None,
        };
        crate::crypto::verify_attestation(network, WORKSPACE_SCOPE, &body)?;
        let expected = crate::crypto::attestation_payload(network, WORKSPACE_SCOPE, &body)?;
        let mut query = self.connection.prepare(
            "SELECT hash,envelope FROM events WHERE network_id=?1 AND scope_id=?2 AND kind='workspace_commanded' AND subject=?3 AND effective=1 ORDER BY lamport,created_at_ms,hash LIMIT 256"
        )?;
        let rows = query.query_map(params![network, WORKSPACE_SCOPE, command_id], |row| {
            Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?))
        })?;
        for row in rows {
            let (hash, envelope) = row?;
            let signed: SignedEvent = serde_json::from_str(&envelope)?;
            let event: Event = serde_json::from_str(&signed.canonical_body)?;
            if event.network_id == network && event.scope_id == WORKSPACE_SCOPE
                && event.body.attestation() == Some(attestation)
                && crate::crypto::attestation_payload(network, WORKSPACE_SCOPE, &event.body)? == expected
            {
                return Ok(json!({"network_id":network,"client_id":client_id,"command_id":command_id,"receipt":"recorded","event_hash":hash}));
            }
        }
        Ok(json!({"network_id":network,"client_id":client_id,"command_id":command_id,"receipt":"not_seen_here"}))
    }
}

impl Store {
    pub fn client_signed_receipt(
        &self,
        network: &str,
        client_id: &str,
        scope_id: &str,
        mut body: Value,
        attestation: &Attestation,
    ) -> Result<Value> {
        vibeface_protocol::validate_id(scope_id).map_err(anyhow::Error::msg)?;
        ensure!(attestation.device_id == client_id, "receipt belongs to another client");
        let object=body.as_object_mut().context("signed body must be an object")?;
        ensure!(!object.contains_key("attestation") && !object.contains_key("cause") && !object.contains_key("acting_assignment"),"receipt needs the original human body without execution authority");
        object.insert("attestation".into(),serde_json::to_value(attestation)?);
        let body:EventBody=serde_json::from_value(body)?;
        crate::crypto::verify_attestation(network,scope_id,&body)?;
        let expected=crate::crypto::attestation_payload(network,scope_id,&body)?;
        let subject=body.subject();
        let mut query=self.connection.prepare(
            "SELECT hash,envelope FROM events WHERE network_id=?1 AND scope_id=?2 AND kind=?3 AND subject IS ?4 AND effective=1 ORDER BY lamport DESC,created_at_ms DESC,hash DESC LIMIT 256"
        )?;
        let rows=query.query_map(params![network,scope_id,body.kind(),subject],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?;
        for row in rows {
            let (hash,envelope)=row?;
            let signed:SignedEvent=serde_json::from_str(&envelope)?;
            let event:Event=serde_json::from_str(&signed.canonical_body)?;
            if event.network_id==network && event.scope_id==scope_id && event.body.attestation()==Some(attestation)
                && crate::crypto::attestation_payload(network,scope_id,&event.body)?==expected {
                return Ok(json!({"network_id":network,"client_id":client_id,"scope_id":scope_id,"receipt":"recorded","event_hash":hash}));
            }
        }
        Ok(json!({"network_id":network,"client_id":client_id,"scope_id":scope_id,"receipt":"not_seen_here"}))
    }
}
