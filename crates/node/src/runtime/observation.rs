use super::*;
use serde::Deserialize;
use vibeface_secure::authorization::{
    Action,
    execution_handoff::{ExecutionHandoffResult, verify_execution_handoff_receipt_signature},
    execution_observation::{
        ExecutionObservationChallenge, ExecutionObservationFreshness, ExecutionObservationPurpose,
        ExecutionObservationRequest, SignedExecutionObservation,
    },
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientObservation {
    method: String,
    observation_request: Signed<ExecutionObservationRequest>,
}

impl Runtime {
    pub async fn forward_client_execution_observation(
        &self,
        client: &VerifiedClientAuthorization,
        body: &Value,
        noise_public_key: &str,
    ) -> Result<Value> {
        let request: ClientObservation = serde_json::from_value(body.clone())?;
        ensure!(
            request.method == "execution_observation",
            "invalid observation method"
        );
        let signed = &request.observation_request;
        let value = &signed.payload.request;
        let intent = &signed.payload.intent;
        let device = DeviceId::from_key(&PublicKey::parse(noise_public_key)?);
        ensure!(
            signed.issuer.as_str() == noise_public_key
                && device == client.scope().device_id
                && value.device == device
                && value.actor == Actor::Account(client.scope().account_id.clone())
                && value.initiator == value.actor
                && value.initiating_grant.is_none()
                && value.context.sdk_session.is_none(),
            "forwarded observation actor differs from the authenticated client"
        );
        ensure!(
            value.version == AUTH_VERSION
                && value.audience == *client.audience()
                && value.presenter == self.audience()?.device
                && value.presenter != value.device
                && value.action == Action::OperationResolve,
            "forwarded observation authorization targets another route"
        );
        ensure!(
            value.resource == intent.target.source.project
                && value.resource.space == client.scope().space_id
                && client
                    .resources()
                    .iter()
                    .any(|record| record.payload.resource == value.resource.resource),
            "forwarded observation resource is outside the verified projection"
        );
        ensure!(
            intent.version == AUTH_VERSION
                && intent.purpose == ExecutionObservationPurpose::Observe
                && value.operation_id == intent.authorization_id
                && value.body_hash == ObjectHash::body(intent)?,
            "forwarded observation differs from signed intent"
        );
        ensure!(
            value.expected_policies.len() <= vibeface_secure::authorization::MAX_POLICY_DOMAINS
                && value
                    .expected_policies
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain)
                && client
                    .heads()
                    .iter()
                    .all(|stamp| value.expected_policies.contains(stamp)),
            "forwarded observation omitted its verified client policy heads"
        );
        let now = now_ms() as u64;
        ensure!(
            value.not_before_ms <= now
                && now < value.expires_at_ms
                && value.expires_at_ms - value.not_before_ms <= INVOCATION_MAX_AGE_MS,
            "forwarded observation expired"
        );
        let verification = SignatureVerification::new();
        verification.run(|| signed.verify_signature())?;
        let target = {
            let store = self.store.lock().await;
            verification.run(|| -> Result<_> {
                for stamp in client.heads() {
                    ensure!(
                        store.authorization_state().head(&stamp.domain) == Some(stamp),
                        "forwarded observation client policy needs synchronization"
                    );
                }
                for stamp in &value.expected_policies {
                    if let Some(current) = store.authorization_state().head(&stamp.domain) {
                        ensure!(
                            current == stamp,
                            "forwarded observation policy needs synchronization"
                        );
                    }
                }
                let certificate = store.authorization_state().verify_service_device(
                    &client.scope().space_id,
                    &value.audience.device,
                    now,
                )?;
                ensure!(
                    certificate.payload.subject == Actor::Service(value.audience.service.clone())
                        && certificate.payload.public_key == *client.audience_public_key(),
                    "forwarded observation target differs from the verified service pin"
                );
                crypto::fingerprint(certificate.payload.public_key.as_str())
            })?
        };
        self.peer_request(
            client.scope().space_id.as_str(),
            &target,
            json!({"type":"execution_observation","authorization_body":body}),
        )
        .await
    }

    pub async fn observe_client_execution(
        self: &Arc<Self>,
        body: Value,
        transport: TransportEvidence,
    ) -> Result<Value> {
        let verification = SignatureVerification::new();
        self.observe_execution_with_verification(body, transport, &verification)
            .await
    }

    async fn observe_execution_with_verification(
        self: &Arc<Self>,
        body: Value,
        transport: TransportEvidence,
        verification: &SignatureVerification,
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
                TransportEvidence::VerifiedPeer { origin } => {
                    origin == &invocation.presenter && origin != &invocation.device
                }
                TransportEvidence::LocalAgentAdapter { .. } => false,
            },
            "observation presenter differs from the authenticated client transport"
        );
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
        if message.payload["type"] == "execution_observation" {
            let body = message.payload["authorization_body"].clone();
            let request: ClientObservation = serde_json::from_value(body.clone())?;
            ensure!(
                request
                    .observation_request
                    .payload
                    .request
                    .resource
                    .space
                    .as_str()
                    == message.network_id,
                "observation route differs from its signed Project"
            );
            return self
                .observe_execution_with_verification(
                    body,
                    TransportEvidence::VerifiedPeer { origin },
                    verification,
                )
                .await;
        }
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
