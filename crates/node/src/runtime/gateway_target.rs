use super::{
    RequestTransport, Runtime, SignatureVerification, TaskScopedPrepareAuthorization, now_ms,
};
use crate::client::GatewayForwardReply;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::sync::Arc;
use vibeface_secure::authorization::{
    Action, PublicKey, Signed,
    gateway_forward::{
        GatewayForwardHeader, GatewayOriginal, GatewayOutcome, sign_gateway_reply,
        verify_directed_gateway,
    },
    node_execution_policy::ActualPolicyNoisePeer,
};

impl Runtime {
    pub(crate) async fn gateway_target_request(
        self: &Arc<Self>,
        actual: &ActualPolicyNoisePeer,
        header: Signed<GatewayForwardHeader>,
        original: GatewayOriginal,
    ) -> Result<GatewayForwardReply> {
        let verification = SignatureVerification::new();
        let local = PublicKey::parse(&self.identity.public_key_hex()?)?;
        let directed = verification.run(|| {
            verify_directed_gateway(
                actual.public_key(),
                &header,
                &original,
                &local,
                now_ms() as u64,
            )
        })?;
        let transport = RequestTransport::DirectedGateway {
            verified: directed.clone(),
            original: original.clone(),
        };
        let result: Result<Value> = async {
            match &original {
                GatewayOriginal::Prepare { body, intent } => {
                    let prepared = {
                        let store = self.store.lock().await;
                        verification.run(|| -> Result<_> {
                            let snapshot = store.connection.unchecked_transaction()?;
                            let current = crate::identity::journal::PolicyState::reload_verified(
                                &snapshot,
                                &store.policy,
                            )?;
                            let prepared = TaskScopedPrepareAuthorization::verify_in(
                                &snapshot,
                                &current,
                                &local,
                                intent.clone(),
                                body,
                                &self.audience()?,
                                transport,
                                now_ms() as u64,
                            )?;
                            snapshot.commit()?;
                            Ok(prepared)
                        })?
                    };
                    self.prepare_authorized_body(prepared, &verification).await
                }
                GatewayOriginal::Invoke { body, invocation }
                | GatewayOriginal::Status { body, invocation } => {
                    if matches!(&original, GatewayOriginal::Status { .. }) {
                        ensure!(
                            invocation.payload.action == Action::OperationStatusRead
                                && matches!(
                                    body["method"].as_str(),
                                    Some(
                                        "operation"
                                            | "session"
                                            | "nodes"
                                            | "remote_execution_seal_status"
                                    )
                                ),
                            "gateway status requires a read-only status method"
                        );
                    }
                    let access = self
                        .authorize_rpc_for_transport(
                            body.clone(),
                            invocation.clone(),
                            transport,
                            verification.clone(),
                        )
                        .await?;
                    self.rpc(access, body.clone()).await
                }
            }
        }
        .await;
        let outcome = match result {
            Ok(value) => GatewayOutcome::Ok(value),
            Err(error) => GatewayOutcome::Error(json!({ "error": format!("{error:#}") })),
        };
        let signer = vibeface_secure::Identity::from_secret_hex(&self.identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let reply = verification
            .run(|| sign_gateway_reply(&directed, &original, &outcome, &signer, now_ms() as u64))?;
        Ok(GatewayForwardReply {
            forward: header,
            reply,
            outcome,
        })
    }
}
