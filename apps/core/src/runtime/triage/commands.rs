use super::super::{field, id};
use super::{Runtime, dequeue, lock, now_ms, shared_event, subject};
use crate::{
    model::{AttentionKind, AttentionRequest, Choice, EventBody, QuestionAction, Severity},
    triage::{self, AnswerResponse, RequestKind, SealedAgentEvent},
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

mod held_answer;
use held_answer::HeldAnswerContext;

struct TriageCommand<'a> {
    attempt: Option<&'a str>,
    review: ReviewCommand<'a>,
}

enum ReviewCommand<'a> {
    Requests(TargetNode<'a>),
    Respond(ReviewResponse<'a>),
    Events { since: i64 },
    Conclude(ReviewConclusion<'a>),
}

#[derive(Clone, Copy)]
enum TargetNode<'a> {
    Unspecified,
    Named(&'a str),
    Invalid,
}

struct ReviewResponse<'a> {
    node: TargetNode<'a>,
    request_id: Result<&'a str>,
    result: Option<&'a Value>,
}

struct ReviewConclusion<'a> {
    event_id: Result<String>,
    action: EventConclusion<'a>,
    logged_severity: Option<&'a str>,
}

enum EventConclusion<'a> {
    Raise(RaiseQuestion<'a>),
    Dismiss,
    Unknown,
}

struct RaiseQuestion<'a> {
    severity: Result<Severity>,
    choices: Result<Vec<String>>,
    request_id: Option<&'a str>,
    text: Result<&'a str>,
    attention: Result<Option<AttentionKind>>,
}

impl<'a> TriageCommand<'a> {
    fn decode(input: &'a Value) -> Result<Self> {
        let action = field(input, "action")?;
        let attempt = input
            .get("_attempt")
            .map(|value| value.as_str().context("invalid triage attempt context"))
            .transpose()?;
        let node = match input.get("node") {
            None => TargetNode::Unspecified,
            Some(Value::String(node)) => TargetNode::Named(node),
            Some(_) => TargetNode::Invalid,
        };
        let review = match action {
            "requests" => ReviewCommand::Requests(node),
            "respond" => ReviewCommand::Respond(ReviewResponse {
                node,
                request_id: field(input, "request_id"),
                result: input.get("result"),
            }),
            "events" => ReviewCommand::Events {
                since: input["since"].as_i64().unwrap_or(0),
            },
            _ => {
                // The handled result takes precedence over validation of a new conclusion.
                let action = match action {
                    "raise" => EventConclusion::Raise(RaiseQuestion {
                        severity: serde_json::from_value(input["severity"].clone())
                            .context("severity must be info, attention or urgent"),
                        choices: serde_json::from_value(
                            input.get("choices").cloned().unwrap_or(json!([])),
                        )
                        .context("choices must be labels"),
                        request_id: input["request_id"].as_str(),
                        text: field(input, "question"),
                        attention: match input["attention_kind"].as_str() {
                            Some("browser_takeover") => Ok(Some(AttentionKind::BrowserTakeover)),
                            Some("approval") => Ok(Some(AttentionKind::Approval)),
                            Some("question") => Ok(Some(AttentionKind::Question)),
                            Some(_) => Err(anyhow::anyhow!("unknown attention kind")),
                            None => Ok(None),
                        },
                    }),
                    "dismiss" => EventConclusion::Dismiss,
                    _ => EventConclusion::Unknown,
                };
                ReviewCommand::Conclude(ReviewConclusion {
                    event_id: id(input, "event_id"),
                    action,
                    logged_severity: input["severity"].as_str(),
                })
            }
        };
        Ok(Self { attempt, review })
    }

    fn serializes_mutation(&self) -> bool {
        match &self.review {
            ReviewCommand::Respond(_) => self.attempt.is_some(),
            ReviewCommand::Conclude(conclusion) => {
                !matches!(conclusion.action, EventConclusion::Unknown)
            }
            ReviewCommand::Requests(_) | ReviewCommand::Events { .. } => false,
        }
    }
}

enum TriageContext<'a> {
    Review(ReviewContext<'a>),
    HeldAnswer(HeldAnswerContext<'a>),
}

#[derive(Clone, Copy)]
enum ReviewContext<'a> {
    Unclaimed,
    ActiveReview(&'a str),
    InactiveReview,
}

impl<'a> ReviewContext<'a> {
    fn attempt(self) -> Result<Option<&'a str>> {
        match self {
            Self::Unclaimed => Ok(None),
            Self::ActiveReview(attempt) => Ok(Some(attempt)),
            Self::InactiveReview => bail!("triage attempt is no longer active"),
        }
    }
}

impl Runtime {
    /// `/api/triage`, the Agents' side of observation review and held request answers.
    pub(crate) async fn triage_command(&self, input: Value) -> Result<Value> {
        let network = self.network_id().await?;
        let command = TriageCommand::decode(&input)?;
        let _mutations = if command.serializes_mutation() {
            Some(self.triage.mutations.lock().await)
        } else {
            None
        };
        let active = lock(&self.triage.active).clone();
        let control = if let Some(attempt) = command.attempt {
            let attempt = attempt.to_owned();
            let boot = self.boot.clone();
            self.store
                .call("triage.answer_control_attempt", move |store| {
                    store.answer_control_attempt(&boot, &attempt)
                })
                .await?
        } else {
            None
        };
        let context = match (command.attempt, control.as_ref()) {
            (None, _) => TriageContext::Review(ReviewContext::Unclaimed),
            (Some(attempt), None) if active.as_deref() == Some(attempt) => {
                TriageContext::Review(ReviewContext::ActiveReview(attempt))
            }
            (Some(_), None) => TriageContext::Review(ReviewContext::InactiveReview),
            (Some(attempt), Some(control)) => {
                TriageContext::HeldAnswer(HeldAnswerContext { attempt, control })
            }
        };
        match context {
            TriageContext::Review(review) => {
                self.handle_review_command(&network, review, active, command.review)
                    .await
            }
            TriageContext::HeldAnswer(context) => {
                self.handle_held_answer_command(context, active.as_deref(), command.review)
                    .await
            }
        }
    }

    async fn handle_review_command(
        &self,
        network: &str,
        review: ReviewContext<'_>,
        active: Option<String>,
        command: ReviewCommand<'_>,
    ) -> Result<Value> {
        match command {
            ReviewCommand::Requests(_) => {
                let mut requests: Vec<_> = lock(&self.triage.requests).values().cloned().collect();
                if let Some(attempt) = review.attempt()? {
                    let events = self.review_events(Some(attempt)).await?;
                    requests.retain(|request| {
                        events
                            .iter()
                            .any(|event| event.request_id().as_deref() == Some(&request.request_id))
                    });
                }
                Ok(json!({"requests":requests}))
            }
            ReviewCommand::Respond(response) => {
                self.respond_to_review(network, review, response).await
            }
            ReviewCommand::Events { since } => self.list_review_events(review, active, since).await,
            ReviewCommand::Conclude(conclusion) => {
                self.conclude_review(network, review, conclusion).await
            }
        }
    }

    async fn respond_to_review(
        &self,
        network: &str,
        review: ReviewContext<'_>,
        response: ReviewResponse<'_>,
    ) -> Result<Value> {
        let request_id = response.request_id?;
        let result = response.result.context("result is required")?.clone();
        if let ReviewContext::Unclaimed = review {
            if let TargetNode::Named(node) = response.node
                && node != self.identity.device_id()?
            {
                return self
                    .peer_request(
                        network,
                        node,
                        json!({"type":"respond","request_id":request_id,"result":result}),
                    )
                    .await;
            }
            return self.respond_unclaimed_request(request_id, result).await;
        }
        if let Some(done) = lock(&self.triage.answered).get(request_id) {
            return Ok(done.clone());
        }
        let attempt = review.attempt()?;
        if let TargetNode::Named(node) = response.node {
            ensure!(
                self.identity.device_id().is_ok_and(|local| node == local),
                "a triage attempt only answers its local requests"
            );
        }
        ensure!(
            self.review_events(attempt)
                .await?
                .iter()
                .any(|event| event.request_id().as_deref() == Some(request_id)),
            "request is not available to this triage attempt"
        );
        self.answer_request(request_id, Some(Ok(result)), "agent")
            .await
    }

    async fn review_events(&self, attempt: Option<&str>) -> Result<Vec<SealedAgentEvent>> {
        let boot = self.boot.clone();
        let attempt = attempt.map(str::to_owned);
        self.store
            .call("triage.claim_events", move |store| {
                store.triage_events(attempt.as_deref().map(|id| (boot.as_str(), id)))
            })
            .await
    }

    async fn list_review_events(
        &self,
        review: ReviewContext<'_>,
        active: Option<String>,
        since: i64,
    ) -> Result<Value> {
        let boot = self.boot.clone();
        let attempt = review.attempt()?.map(str::to_owned);
        self.store
            .call("triage.events", move |store| {
                let events: Vec<_> = store
                    .triage_events(attempt.as_deref().map(|id| (boot.as_str(), id)))?
                    .into_iter()
                    .filter(|event| event.observation().ts_ms >= since)
                    .collect();
                let unknown =
                    store.unknown_triage_events(active.as_deref().map(|id| (boot.as_str(), id)))?;
                Ok(json!({"events":events,"unknown_events":unknown}))
            })
            .await
    }

    async fn conclude_review(
        &self,
        network: &str,
        review: ReviewContext<'_>,
        conclusion: ReviewConclusion<'_>,
    ) -> Result<Value> {
        let event_id = conclusion.event_id?;
        ensure!(
            !event_id.starts_with("answer:"),
            "answer controls are private to their claimed request"
        );
        {
            let event_id = event_id.clone();
            let handled: Option<String> = self
                .store
                .call("triage.handled_result", move |store| {
                    Ok(store
                        .connection
                        .query_row(
                            "SELECT result FROM triage_handled WHERE event_id=?",
                            [&event_id],
                            |r| r.get(0),
                        )
                        .optional()?)
                })
                .await?;
            if let Some(handled) = handled {
                return Ok(serde_json::from_str(&handled)?);
            }
        }
        let event = self
            .review_events(review.attempt()?)
            .await?
            .into_iter()
            .find(|event| event.event_id().as_str() == event_id.as_str())
            .context("no pending event with this ID")?;
        let (action, result) = match conclusion.action {
            EventConclusion::Dismiss => ("dismiss", json!({"event_id":event_id,"dismissed":true})),
            EventConclusion::Raise(question) => {
                let question = self.review_question(&event, question)?;
                ("raise", self.raise(network, &event, question).await?)
            }
            EventConclusion::Unknown => bail!("unknown triage action"),
        };
        let (provider, session) = (
            event.provider().name(),
            event.observation().session_id.clone(),
        );
        let handled = event_id.clone();
        let result = self
            .store
            .call("triage.handle_event", move |store| {
                store.connection.execute(
                    "INSERT INTO triage_handled(event_id,ts_ms,result) VALUES(?,?,?)
                 ON CONFLICT(event_id) DO UPDATE SET ts_ms=excluded.ts_ms,result=excluded.result",
                    params![event_id, now_ms(), result.to_string()],
                )?;
                dequeue(&store.connection, &event)?;
                Ok(result)
            })
            .await?;
        // What was concluded, never the question or the session's words.
        tracing::info!(
            event = "triage.concluded",
            conversation_id = result["conversation_id"].as_str(),
            thread_id = session,
            event_id = handled,
            provider,
            conclusion = action,
            severity = conclusion.logged_severity,
            "triage concluded"
        );
        Ok(result)
    }

    fn review_question(
        &self,
        event: &SealedAgentEvent,
        question: RaiseQuestion<'_>,
    ) -> Result<EventBody> {
        let severity = question.severity?;
        let labels = question.choices?;
        let request_id = question.request_id.map(str::to_owned);
        if let Some(request_id) = &request_id {
            ensure!(
                event.request_id().as_deref() == Some(request_id),
                "request does not belong to this event"
            );
            ensure!(
                lock(&self.triage.requests).contains_key(request_id),
                "no request is waiting with this ID"
            );
        }
        let held_request = request_id
            .as_ref()
            .and_then(|id| lock(&self.triage.requests).get(id).cloned());
        let approval = held_request
            .as_ref()
            .and_then(|request| match &request.response {
                AnswerResponse::Approval { allow_once, .. } => Some(allow_once.is_some()),
                _ => None,
            });
        let choices = match approval {
            Some(allow_once) => {
                let mut choices = Vec::new();
                if allow_once {
                    choices.push(Choice {
                        label: "允许本次".into(),
                        action: Some(QuestionAction::AllowOnce),
                    });
                }
                choices.push(Choice {
                    label: "拒绝".into(),
                    action: Some(QuestionAction::Deny),
                });
                choices
            }
            None => labels
                .into_iter()
                .map(|label| Choice {
                    label: crate::logging::scrub(&label),
                    action: None,
                })
                .collect(),
        };
        Ok(EventBody::QuestionAsked {
            question_id: triage::question_id(event.node(), event.event_id()),
            target: None,
            attention: AttentionRequest {
                kind: if approval.is_some() {
                    AttentionKind::Approval
                } else {
                    question.attention?.unwrap_or(
                        if event.observation().hold == Some(RequestKind::Permission) {
                            AttentionKind::Approval
                        } else {
                            AttentionKind::Question
                        },
                    )
                },
                device_id: Some(event.node().clone()),
            },
            severity,
            text: crate::logging::scrub(question.text?),
            choices,
            about: subject(
                &shared_event(event, &self.settings_snapshot().roots),
                request_id,
            ),
        })
    }
}
