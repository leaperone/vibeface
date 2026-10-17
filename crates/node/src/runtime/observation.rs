use super::*;
use serde::Deserialize;
use vibeface_secure::authorization::{
    execution_handoff::{ExecutionHandoffResult, verify_execution_handoff_receipt_signature},
    execution_observation::{
        ExecutionObservationChallenge, ExecutionObservationFreshness, ExecutionObservationRequest,
        SignedExecutionObservation,
    },
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientObservation {
    method: String,
    observation_request: Signed<ExecutionObservationRequest>,
}

impl Runtime {
    pub async fn observe_client_execution(
        self: &Arc<Self>,
        body: Value,
        transport: TransportEvidence,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        let request: ClientObservation = serde_json::from_value(body)?;
        ensure!(
            request.method == "execution_observation",
            "invalid observation method"
        );
        let invocation = &request.observation_request.payload.request;
        ensure!(
            match &transport {
                TransportEvidence::DirectNoise { device } => {
                    device == &invocation.device && device == &invocation.presenter
                }
                TransportEvidence::GatewayNoise { device, gateway } => {
                    device == &invocation.device && gateway == &invocation.presenter
                }
                _ => false,
            },
            "observation presenter differs from the authenticated client transport"
        );
        let verification = SignatureVerification::new();
        let (wire, original, target) = {
            let store = self.store.lock().await;
            verification.run(|| -> Result<_> {
                let wire = store.prepare_execution_dispatch_observation(
                    &self.identity,
                    &request.observation_request,
                    &transport,
                    now_ms() as u64,
                )?;
                let target = &wire.request.payload.intent.target;
                let original = store
                    .execution_dispatch_handoff(
                        target.source.project.space.as_str(),
                        &target.source.operation_id,
                    )?
                    .context("observation has no durable original handoff")?;
                let peer = crypto::fingerprint(target.target_public_key.as_str())?;
                Ok((wire, original, peer))
            })?
        };
        let source = &wire.request.payload.intent.target.source;
        let response = self
            .peer_request(
                source.project.space.as_str(),
                &target,
                json!({"type":"execution_handoff_observe","observation":wire}),
            )
            .await?;
        ensure!(
            response.get("actual").is_some(),
            "executor did not return an observation result"
        );
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ObservationResult {
            actual: Option<ExecutionHandoffResult>,
        }
        let response: ObservationResult = serde_json::from_value(response)?;
        let store = self.store.lock().await;
        verification.run(|| -> Result<Value> {
            store.prepare_execution_dispatch_observation(
                &self.identity,
                &request.observation_request,
                &transport,
                now_ms() as u64,
            )?;
            ensure!(
                store
                    .execution_dispatch_handoff(
                        source.project.space.as_str(),
                        &source.operation_id
                    )?
                    .as_ref()
                    == Some(&original),
                "observed handoff changed while its result was being read"
            );
            if let Some(actual) = &response.actual {
                ensure!(
                    actual.handoff == original,
                    "observation changed its original handoff"
                );
                verify_execution_handoff_receipt_signature(
                    &original,
                    &actual.receipt,
                    &actual.result,
                )?;
                ensure!(
                    actual.receipt.payload.issued_at_ms <= now_ms() as u64,
                    "observation receipt is from the future"
                );
            }
            let operation = store
                .operation(source.project.space.as_str(), &source.operation_id)?
                .context("observed Project operation is missing")?;
            ensure!(
                operation.request.target_node == target,
                "observation targets another execution host"
            );
            let operation = dispatch::execution_operation_view(&store, &operation)?;
            Ok(json!({
                "execution_source":operation["execution_source"], "operation":operation,
                "original_handoff":original, "actual":response.actual, "remote_verified":true,
            }))
        })
    }

    pub(super) async fn handle_execution_observation_peer(
        self: &Arc<Self>,
        message: &Inbound,
        origin: DeviceId,
        verification: &SignatureVerification,
    ) -> Result<Value> {
        if message.payload["type"] == "execution_handoff_observation_receipts" {
            let challenge: ExecutionObservationChallenge =
                serde_json::from_value(message.payload["challenge"].clone())?;
            let store = self.store.lock().await;
            let receipts = verification.run(|| {
                store.sign_execution_observation_receipts(
                    &challenge,
                    &self.identity,
                    now_ms() as u64,
                )
            })?;
            return Ok(json!({"receipts":receipts}));
        }
        let _work = self.maintenance.enter()?;
        let wire: SignedExecutionObservation =
            serde_json::from_value(message.payload["observation"].clone())?;
        let target = &wire.request.payload.intent.target;
        ensure!(
            wire.invocation.payload.device == origin && wire.invocation.payload.presenter == origin,
            "observation presenter differs from the authenticated peer"
        );
        ensure!(
            target.source.project.space.as_str() == message.network_id
                && target.execution_audience == self.audience()?
                && target.target_public_key.as_str() == self.identity.public_key_hex()?,
            "observation reached another Project or executor"
        );
        let transport = TransportEvidence::VerifiedPeer { origin };
        let (challenge, receipts) = {
            let mut store = self.store.lock().await;
            verification.run(|| -> Result<_> {
                let challenge = store.issue_execution_observation_challenge(
                    &self.identity,
                    &wire,
                    &transport,
                    now_ms() as u64,
                )?;
                let receipts = store.sign_execution_observation_receipts(
                    &challenge,
                    &self.identity,
                    now_ms() as u64,
                )?;
                Ok((challenge, receipts))
            })?
        };
        let receipts = self
            .collect_handoff_receipts(
                &target.source.project.space,
                &challenge.policies,
                receipts,
                "execution_handoff_observation_receipts",
                json!(challenge),
            )
            .await?;
        let proof = ExecutionObservationFreshness {
            challenge,
            receipts,
        };
        let store = self.store.lock().await;
        let actual = verification.run(|| {
            store.observe_execution_handoff(
                &self.identity,
                &wire,
                &proof,
                &transport,
                now_ms() as u64,
            )
        })?;
        Ok(json!({"actual":actual}))
    }
}
