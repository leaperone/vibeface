//! Questions live in the chat of the bot that owns their session. A question's one answer is
//! the user message `a-<question_id>`; a question is unanswered while no such message exists.
//! The device that asked may close an unanswered one it can no longer act on; an answer that
//! stands wins over a close, in any order they meet.
//! A session gets its owner from a work bound to it or from the device running it. A deleted
//! bot owns nothing: its sessions, muted or not, are unowned again.
use super::{
    Store, authorize_node_from,
    bots::bot_for_conversation_from,
    conversation_from, log, message_from,
    scheduler::{admit_from, queue_delivery_from},
};
use crate::{crypto::LocalIdentity, model::*, scheduling::*};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

pub fn answer_id(question: &str) -> String {
    format!("a-{question}")
}

/// The outcome of posting an answer: the answer that stands, and its admission when new.
pub struct Answered {
    pub conversation_id: ScopeId,
    pub message: Message,
    pub already_answered: bool,
    pub receipt: Option<AdmissionReceipt>,
}

const QUESTION: &str = "SELECT q.body,q.conversation_id,q.author,q.created_at_ms,q.closed,m.message_id,m.text,m.author,m.created_at_ms FROM questions q LEFT JOIN messages m ON m.network_id=q.network_id AND m.conversation_id=q.conversation_id AND m.answers=q.question_id WHERE q.network_id=?1";

type Row = (
    String,
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
);

/// Questions as clients read them, with the bot they belong to and their answer.
fn questions_from(
    connection: &Connection,
    network: &str,
    filter: &str,
    values: impl rusqlite::Params,
) -> Result<Vec<Value>> {
    let mut statement = connection.prepare(&format!("{QUESTION} {filter} ORDER BY q.rank"))?;
    let rows: Vec<Row> = statement
        .query_map(values, |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(
            |(body, scope, author, created_at_ms, closed, answer, text, by, at)| {
                let mut question: Value = serde_json::from_str(&body)?;
                if let Some(object) = question.as_object_mut() {
                    object.remove("type");
                }
                let bot = bot_for_conversation_from(connection, network, &scope)?
                    .map(|bot| json!({"bot_id":bot.bot_id,"name":bot.name}));
                question["conversation_id"] = json!(scope);
                question["bot"] = json!(bot);
                question["author"] = json!(author);
                question["created_at_ms"] = json!(created_at_ms);
                (question["answer"], question["closed"]) = match answer {
                    Some(message_id) => (
                        json!({"message_id":message_id,"text":text,"author":by,"created_at_ms":at}),
                        Value::Null,
                    ),
                    None => (Value::Null, json!(closed)),
                };
                Ok(question)
            },
        )
        .collect()
}

pub(super) fn question_from(
    connection: &Connection,
    network: &str,
    question: &str,
) -> Result<Option<Value>> {
    Ok(questions_from(
        connection,
        network,
        "AND q.question_id=?2",
        params![network, question],
    )?
    .pop())
}

/// The `session_owners` row of `(?1, ?2, ?3)` while its bot is not deleted.
const OWNED: &str = "FROM session_owners s WHERE s.network_id=?1 AND s.node=?2 AND s.session_id=?3
    AND NOT EXISTS (SELECT 1 FROM bot_deletions d WHERE d.network_id=s.network_id AND d.conversation_id=s.conversation_id)";

pub(crate) fn session_owner_from(
    connection: &Connection,
    network: &str,
    node: &str,
    session: &str,
) -> Result<Option<ScopeId>> {
    Ok(connection
        .query_row(
            &format!("SELECT conversation_id {OWNED}"),
            params![network, node, session],
            |r| r.get(0),
        )
        .optional()?)
}

impl Store {
    pub fn session_owner(
        &self,
        network: &str,
        node: &str,
        session: &str,
    ) -> Result<Option<ScopeId>> {
        session_owner_from(&self.connection, network, node, session)
    }

    pub fn question(&self, network: &str, question: &str) -> Result<Option<Value>> {
        question_from(&self.connection, network, question)
    }

    /// The question `node` asked about its App Server request `request`, in whatever state.
    pub fn request_question(
        &self,
        network: &str,
        node: &str,
        request: &str,
    ) -> Result<Option<Value>> {
        Ok(questions_from(
            &self.connection,
            network,
            "AND q.author=?2 AND q.node=?2 AND q.request_id=?3",
            params![network, node, request],
        )?
        .pop())
    }

    /// Every open question: no one answered it and its device did not close it.
    pub fn unanswered_questions(&self, network: &str) -> Result<Vec<Value>> {
        questions_from(
            &self.connection,
            network,
            "AND m.message_id IS NULL AND q.closed IS NULL",
            params![network],
        )
    }

    /// One conversation's questions with their answers, in chat order.
    pub fn conversation_questions(&self, network: &str, scope: &str) -> Result<Vec<Value>> {
        questions_from(
            &self.connection,
            network,
            "AND q.conversation_id=?2",
            params![network, scope],
        )
    }

    /// Sessions the device running them attributed to this conversation's bot, muted ones
    /// included, with the version a hand-over names, for the lines in its chat.
    pub fn attributed_sessions(&self, network: &str, scope: &str) -> Result<Vec<Value>> {
        let mut statement = self.connection.prepare(
            "SELECT node,session_id,attribution,muted,created_at_ms,rank FROM session_owners
             WHERE network_id=? AND conversation_id=? AND attribution IS NOT NULL ORDER BY rank",
        )?;
        let rows: Vec<(String, String, String, bool, i64, String)> = statement
            .query_map(params![network, scope], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(node, session, attribution, muted, created_at_ms, rank)| {
                let mut row: Value = serde_json::from_str(&attribution)?;
                row["node"] = json!(node);
                row["session_id"] = json!(session);
                row["muted"] = json!(muted);
                row["created_at_ms"] = json!(created_at_ms);
                row["version"] = json!(super::version(&rank));
                row["rank"] = json!(rank);
                Ok(row)
            })
            .collect()
    }

    /// Whether its bot stopped following this session.
    pub fn session_muted(&self, network: &str, node: &str, session: &str) -> Result<bool> {
        Ok(self
            .connection
            .query_row(
                &format!("SELECT muted {OWNED}"),
                params![network, node, session],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// Writes `body` once. A question with its ID already stands; in a conversation, so does an
    /// unanswered one about the same session and request. Returns the question and whether it is new.
    pub fn ask(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        body: EventBody,
        created_at_ms: i64,
    ) -> Result<(Value, Option<SignedEvent>)> {
        let EventBody::QuestionAsked {
            question_id, about, ..
        } = &body
        else {
            anyhow::bail!("not a question");
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut standing = question_from(&tx, network, question_id)?;
        if standing.is_none() {
            standing = questions_from(
                &tx,
                network,
                "AND q.node=?2 AND q.session_id=?3 AND q.request_id IS ?4 AND m.message_id IS NULL",
                params![network, about.node, about.session_id, about.request_id],
            )?
            .pop();
        }
        if let Some(standing) = standing {
            return Ok((standing, None));
        }
        let question = question_id.clone();
        let event = log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            scope,
            body,
            vec![],
            created_at_ms,
        )?;
        let asked = question_from(&tx, network, &question)?.context("question missing")?;
        tx.commit()?;
        Ok((asked, Some(event)))
    }

    /// Closes this device's open questions whose App Server request is not in `held`, the
    /// requests waiting now. Returns each close it wrote with its conversation; another call
    /// writes no more for them.
    pub fn close_lost_questions(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        held: &std::collections::HashSet<String>,
        created_at_ms: i64,
    ) -> Result<Vec<(ScopeId, SignedEvent)>> {
        let node = identity.device_id()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let lost: Vec<(String, String, String)> = tx
            .prepare(
                "SELECT q.question_id,q.conversation_id,q.request_id FROM questions q
                 WHERE q.network_id=?1 AND q.author=?2 AND q.node=?2 AND q.request_id IS NOT NULL AND q.closed IS NULL
                 AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.network_id=q.network_id AND m.conversation_id=q.conversation_id AND m.answers=q.question_id)
                 ORDER BY q.rank",
            )?
            .query_map(params![network, node], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut written = Vec::new();
        for (question, scope, request) in lost {
            if held.contains(&request) {
                continue;
            }
            let event = log::append_from(
                &tx,
                &self.blob_dir,
                identity,
                network,
                &scope,
                EventBody::QuestionClosed {
                    question_id: question,
                    reason: CloseReason::RequestLost,
                },
                vec![],
                created_at_ms,
            )?;
            written.push((scope, event));
        }
        tx.commit()?;
        Ok(written)
    }

    /// Gives a session this device runs and no bot owns to `conversation`. Writes nothing
    /// once a bot owns the session or muted it, so a repeated event or a restart does not
    /// attribute it again; once that bot is deleted, the next call gives it away again.
    pub fn claim_session(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        conversation: &str,
        about: &Subject,
        created_at_ms: i64,
    ) -> Result<Option<SignedEvent>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            about.node == identity.device_id()?,
            "the device running the session attributes it"
        );
        let agent = about.agent.context("only an agent session is attributed")?;
        if session_owner_from(&tx, network, &about.node, &about.session_id)?.is_some() {
            return Ok(None);
        }
        let event = log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            conversation,
            EventBody::SessionAttributed {
                node: about.node.clone(),
                agent,
                session_id: about.session_id.clone(),
                cwd: about.cwd.clone(),
                summary: about.summary.clone(),
            },
            vec![],
            created_at_ms,
        )?;
        tx.commit()?;
        Ok(Some(event))
    }

    /// `from`'s bot hands an attributed session this device runs to `target`, or mutes it in
    /// its own conversation when `None`. Only the bot that owns the session changes it, and only
    /// at the version `seen` it read, so a retry after the session moved on writes nothing;
    /// asking for the state that stands writes nothing, so a retry reads it back. Returns the
    /// owner before, the version after, and the event when this call wrote one.
    #[expect(
        clippy::too_many_arguments,
        reason = "a hand-over names the session, the version it read and the new owner"
    )]
    pub fn hand_over_session(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        from: &str,
        session: &str,
        seen: &str,
        target: Option<&str>,
        created_at_ms: i64,
    ) -> Result<(ScopeId, String, Option<SignedEvent>)> {
        let node = identity.device_id()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (owner, attribution, muted, rank): (String, Option<String>, bool, String) = tx
            .query_row(
                &format!("SELECT conversation_id,attribution,muted,rank {OWNED}"),
                params![network, node, session],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .context("no bot follows this session on this device")?;
        let version = super::version(&rank);
        match target {
            Some(target) if target == owner && !muted => return Ok((owner, version, None)),
            None if muted && owner == from => return Ok((owner, version, None)),
            _ if owner != from => {
                let name =
                    bot_for_conversation_from(&tx, network, &owner)?.map_or(owner, |bot| bot.name);
                bail!("this session is not yours; {name} follows it")
            }
            _ => {}
        }
        ensure!(
            seen == version,
            "this session changed since version {seen}; it is at version {version} now"
        );
        let attribution: Value = serde_json::from_str(
            &attribution.context("this session belongs to the work that started it")?,
        )?;
        let body = match target {
            Some(target) => {
                ensure!(
                    bot_for_conversation_from(&tx, network, target)?.is_some(),
                    "no such bot"
                );
                let text = |key: &str| attribution[key].as_str().unwrap_or_default().to_owned();
                EventBody::SessionAttributed {
                    node: node.clone(),
                    agent: serde_json::from_value(attribution["agent"].clone())?,
                    session_id: session.into(),
                    cwd: text("cwd"),
                    summary: text("summary"),
                }
            }
            None => EventBody::SessionMuted {
                node: node.clone(),
                session_id: session.into(),
            },
        };
        let scope = target.unwrap_or(&owner).to_owned();
        let event = log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            &scope,
            body,
            vec![],
            created_at_ms,
        )?;
        tx.commit()?;
        Ok((owner, super::version(&event.hash), Some(event)))
    }

    /// Posts the one answer to `question`. A retry with the same text returns it; a different
    /// text finds the answer that stands. A new answer in a conversation is admitted on the owner,
    /// or kept for delivery to it, as an `answer` turn.
    pub fn accept_answer(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        question: &str,
        text: &str,
        attestation: Option<Attestation>,
        created_at_ms: i64,
    ) -> Result<Answered> {
        let local = identity.device_id()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network, &local)?;
        let (scope, closed): (String, Option<String>) = tx
            .query_row(
                "SELECT conversation_id,closed FROM questions WHERE network_id=? AND question_id=?",
                params![network, question],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .context("no such question on this device")?;
        let message_id = answer_id(question);
        if let Some(message) = message_from(&tx, network, &scope, &message_id)? {
            return Ok(Answered {
                already_answered: message.text != text,
                conversation_id: scope,
                message,
                receipt: None,
            });
        }
        if let Some(reason) = closed {
            bail!("this question was closed ({reason}); nothing can act on an answer now");
        }
        log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            &scope,
            EventBody::MessagePosted {
                message_id: message_id.clone(),
                role: Role::User,
                text: text.into(),
                attachments: vec![],
                report_results: Some(true),
                in_reply_to: None,
                status: None,
                answers: Some(question.into()),
                attestation,
                failure: None,
            },
            vec![],
            created_at_ms,
        )?;
        let trigger = message_trigger(&message_id);
        let owner = conversation_from(&tx, network, &scope)?
            .and_then(|conversation| conversation.owner)
            .context("conversation has no scheduling Node")?;
        let receipt = Some(if owner == local {
            admit_from(
                &tx,
                network,
                &scope,
                &trigger,
                TriggerKind::Answer,
                &message_id,
            )?
        } else {
            queue_delivery_from(&tx, network, &scope, &trigger)?
        });
        let message = message_from(&tx, network, &scope, &message_id)?.context("answer missing")?;
        tx.commit()?;
        Ok(Answered {
            conversation_id: scope,
            message,
            already_answered: false,
            receipt,
        })
    }
}
