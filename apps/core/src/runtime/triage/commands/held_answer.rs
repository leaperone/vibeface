use super::{ReviewCommand, TargetNode};
use crate::runtime::triage::{
    AGENT_ANSWER_WITHIN, HeldRequest, Runtime, lock, validate_answer_result,
};
use crate::{
    manager,
    model::Message,
    triage::{AnswerResponse, HeldAnswerControl},
};
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};

#[derive(Clone, Copy)]
pub(super) struct HeldAnswerContext<'a> {
    pub(super) attempt: &'a str,
    pub(super) control: &'a HeldAnswerControl,
}

enum HeldAnswerCommand<'a> {
    Requests,
    Respond(&'a Value),
}

impl HeldAnswerContext<'_> {
    fn ensure_active(self, active: Option<&str>) -> Result<()> {
        ensure!(
            active == Some(self.attempt),
            "answer control is no longer active"
        );
        Ok(())
    }

    fn bind<'a>(
        self,
        command: ReviewCommand<'a>,
        active: Option<&str>,
    ) -> Result<HeldAnswerCommand<'a>> {
        let (node, response) = match command {
            ReviewCommand::Requests(node) => (node, None),
            ReviewCommand::Respond(response) => (response.node, Some(response)),
            _ => bail!("answer controls only read and respond to their original request"),
        };
        self.ensure_active(active)?;
        ensure!(
            match node {
                TargetNode::Unspecified => true,
                TargetNode::Named(node) => node == self.control.request.node,
                TargetNode::Invalid => false,
            },
            "answer controls cannot target another Node"
        );
        match response {
            None => Ok(HeldAnswerCommand::Requests),
            Some(response) => {
                ensure!(
                    response.request_id? == self.control.request.request_id,
                    "answer control cannot respond to another request"
                );
                Ok(HeldAnswerCommand::Respond(
                    response.result.context("result is required")?,
                ))
            }
        }
    }
}

impl Runtime {
    pub(super) async fn handle_held_answer_command(
        &self,
        context: HeldAnswerContext<'_>,
        active: Option<&str>,
        command: ReviewCommand<'_>,
    ) -> Result<Value> {
        let command = context.bind(command, active)?;
        self.handle_held_answer(context, command).await
    }

    async fn handle_held_answer(
        &self,
        context: HeldAnswerContext<'_>,
        command: HeldAnswerCommand<'_>,
    ) -> Result<Value> {
        let result = match command {
            HeldAnswerCommand::Requests => {
                let request = self.validate_live_answer_control(context).await?;
                return Ok(json!({"requests":[request]}));
            }
            HeldAnswerCommand::Respond(result) => result,
        };
        let control = context.control;
        let request_id = &control.request.request_id;
        let (question, answer) = self.held_answer_input(context).await?;
        validate_answer_result(&control.request.response, &question, &answer.text, result)?;
        let by = format!("answer:{}", control.answer_hash);
        if let Some(done) = lock(&self.triage.answered).get(request_id) {
            ensure!(
                done["by"] == by,
                "the request was answered by another actor"
            );
            return Ok(done.clone());
        }

        // The request can change while we wait for the configuration lock and permission leases.
        self.validate_live_answer_control(context).await?;
        let _permissions = self.config_save.lock().await;
        let denying = matches!(&control.request.response, AnswerResponse::Approval { deny, .. } if deny == result);
        let _leases = if denying {
            Vec::new()
        } else {
            self.answer_permission_lease(control).await?
        };
        self.validate_live_answer_control(context).await?;
        self.send_request_answer(
            request_id,
            Some(Ok(result.clone())),
            &by,
            Some(&control.request),
        )
        .await
    }

    async fn held_answer_input(&self, context: HeldAnswerContext<'_>) -> Result<(Value, Message)> {
        let control = context.control.clone();
        self.store
            .call("triage.answer_control_input", move |store| {
                store.answer_control_input(&control)
            })
            .await
    }

    async fn validate_live_answer_control(
        &self,
        context: HeldAnswerContext<'_>,
    ) -> Result<HeldRequest> {
        context.ensure_active(lock(&self.triage.active).as_deref())?;
        let HeldAnswerContext { attempt, control } = context;
        let node = self.identity.device_id()?;
        ensure!(
            control.network == self.network_id().await? && control.request.node == node,
            "answer control belongs to another Node or network"
        );
        {
            let boot = self.boot.clone();
            let attempt = attempt.to_owned();
            let control = control.clone();
            self.store
                .call("triage.validate_answer_claim", move |store| {
                    store.validate_answer_control_claim(&boot, &attempt, &control)
                })
                .await?;
        }
        let request = lock(&self.triage.requests)
            .get(&control.request.request_id)
            .cloned()
            .context("the original request is no longer waiting")?;
        ensure!(
            request.question_id.as_deref() == Some(control.question_id.as_str())
                && request.answer_binding(&node) == control.request,
            "the held request or transport changed"
        );
        Ok(request)
    }

    pub(in crate::runtime::triage) async fn run_answer_control(
        &self,
        attempt: &str,
        control: &HeldAnswerControl,
    ) -> Result<()> {
        let context = HeldAnswerContext { attempt, control };
        let request = self.validate_live_answer_control(context).await?;
        let (question, answer) = self.held_answer_input(context).await?;
        if let Some(result) = crate::store::triage::approval_answer_result(
            &question,
            &answer.text,
            &control.request.response,
        )? {
            let _mutations = self.triage.mutations.lock().await;
            context.ensure_active(lock(&self.triage.active).as_deref())?;
            let sent = self
                .handle_held_answer(context, HeldAnswerCommand::Respond(&result))
                .await?;
            ensure!(
                sent["answered"] == true,
                "the original request's response is unconfirmed"
            );
            return Ok(());
        }
        let mut config = request
            .answer_manager
            .context("the original request has no captured coordination backend")?;
        ensure!(
            self.manager().is_some(),
            "coordination Agent permission was revoked"
        );
        drop(self.answer_permission_lease(control).await?);
        self.prepare_manager_login(&mut config)
            .await
            .map_err(manager::ManagerError::not_started)?;
        config.timeout_secs = config.timeout_secs.min(AGENT_ANSWER_WITHIN.as_secs());
        self.run_coordination(config, Some(attempt), |cli| {
            Ok(format!(
                "Deliver this user's stored answer to its original live request. This is a fresh coordination session, not a Task or Main turn. The user's signed message is the sole answer fact. Preserve its text. Do not approve permissions, change tool input, add permissions, create work, or send chat messages. Only use `{cli} requests` and `{cli} respond REQUEST_ID 'RESULT_JSON'` for the bound request. The Node validates the claim, transport generation, original schema, and result. If the schema cannot express the answer, leave the request unanswered. Your final text is discarded.\n{}",
                serde_json::to_string(&json!({"request_id":control.request.request_id,
                    "question":question,"answer":{"message_id":answer.message_id,"text":answer.text},
                    "response_schema":control.request.response}))?
            ))
        }).await?;
        ensure!(
            lock(&self.triage.answered)
                .get(&control.request.request_id)
                .is_some_and(|done| done["answered"] == true
                    && done["by"] == format!("answer:{}", control.answer_hash)),
            "the coordination turn did not confirm the stored answer's delivery"
        );
        Ok(())
    }
}
