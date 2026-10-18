use super::{prepare::PrepareContext, *};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationOpenRef {
    pub operation_id: String,
    pub invocation_hash: ObjectHash,
    pub admission_hash: ObjectHash,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionWork {
    TaskSession {
        task: EventRef,
        binding_hash: String,
        binding_generation: u64,
        execution_session: SessionRef,
    },
    ConversationSession {
        conversation: String,
        execution_session: SessionRef,
        executor: String,
        canonical_cwd: String,
        open: ConversationOpenRef,
    },
    Command {
        request_hash: ObjectHash,
        canonical_cwd: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConversationOpenIntent {
    Opening { operation_id: String },
    Existing { open: ConversationOpenRef },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionWorkIntent {
    TaskSession {
        task: EventRef,
        binding_generation: u64,
        execution_session: SessionRef,
    },
    ConversationSession {
        conversation: String,
        execution_session: SessionRef,
        executor: String,
        canonical_cwd: String,
        open: ConversationOpenIntent,
    },
    Command {
        request_hash: ObjectHash,
        canonical_cwd: String,
    },
}

impl ExecutionWork {
    pub fn task_ref(&self) -> Option<&EventRef> {
        match self {
            Self::TaskSession { task, .. } => Some(task),
            _ => None,
        }
    }

    pub fn execution_session(&self) -> Option<&SessionRef> {
        match self {
            Self::TaskSession {
                execution_session, ..
            }
            | Self::ConversationSession {
                execution_session, ..
            } => Some(execution_session),
            Self::Command { .. } => None,
        }
    }

    pub fn binding_generation(&self) -> Option<u64> {
        match self {
            Self::TaskSession {
                binding_generation, ..
            } => Some(*binding_generation),
            _ => None,
        }
    }

    pub fn binding_hash(&self) -> Option<&str> {
        match self {
            Self::TaskSession { binding_hash, .. } => Some(binding_hash),
            _ => None,
        }
    }

    pub fn canonical_cwd(&self) -> Option<&str> {
        match self {
            Self::ConversationSession { canonical_cwd, .. }
            | Self::Command { canonical_cwd, .. } => Some(canonical_cwd),
            Self::TaskSession { .. } => None,
        }
    }

    pub fn intent(&self) -> ExecutionWorkIntent {
        match self {
            Self::TaskSession {
                task,
                binding_generation,
                execution_session,
                ..
            } => ExecutionWorkIntent::TaskSession {
                task: task.clone(),
                binding_generation: *binding_generation,
                execution_session: execution_session.clone(),
            },
            Self::ConversationSession {
                conversation,
                execution_session,
                executor,
                canonical_cwd,
                open,
            } => ExecutionWorkIntent::ConversationSession {
                conversation: conversation.clone(),
                execution_session: execution_session.clone(),
                executor: executor.clone(),
                canonical_cwd: canonical_cwd.clone(),
                open: ConversationOpenIntent::Existing { open: open.clone() },
            },
            Self::Command {
                request_hash,
                canonical_cwd,
            } => ExecutionWorkIntent::Command {
                request_hash: request_hash.clone(),
                canonical_cwd: canonical_cwd.clone(),
            },
        }
    }

    pub fn validate_context(&self, context: &PrepareContext, target: &PublicKey) -> Result<()> {
        self.intent().validate_context(context, target)
    }
}

impl ExecutionWorkIntent {
    pub fn approves_intent(&self, current: &Self) -> bool {
        match (self, current) {
            (
                Self::ConversationSession {
                    conversation,
                    execution_session,
                    executor,
                    canonical_cwd,
                    open: ConversationOpenIntent::Opening { operation_id },
                },
                Self::ConversationSession {
                    conversation: actual_conversation,
                    execution_session: actual_session,
                    executor: actual_executor,
                    canonical_cwd: actual_cwd,
                    open: ConversationOpenIntent::Existing { open },
                },
            ) => {
                conversation == actual_conversation
                    && execution_session == actual_session
                    && executor == actual_executor
                    && canonical_cwd == actual_cwd
                    && operation_id == &open.operation_id
            }
            _ => self == current,
        }
    }

    pub fn validate_request(
        &self,
        invocation: &Invocation,
        body: &serde_json::Value,
    ) -> Result<()> {
        let request = &body["request"];
        require(
            body["method"] == "operate"
                && request["operation_id"].as_str() == Some(invocation.operation_id.as_str())
                && request["conversation_id"].as_str()
                    == invocation.context.conversation.as_deref(),
            "execution work differs from the signed operation",
        )?;
        match self {
            Self::Command {
                request_hash,
                canonical_cwd,
            } => require(
                *request_hash == ObjectHash::body(request)?
                    && invocation.action == Action::ExecutionCommand
                    && request["action"]["type"] == "command"
                    && request["action"]["cwd"].as_str() == Some(canonical_cwd.as_str())
                    && request["session_id"].is_null()
                    && request["task_id"].is_null()
                    && request["binding"].is_null()
                    && request["expected_revision"].as_u64() == Some(0),
                "command work changed its signed request or gained a session",
            ),
            Self::ConversationSession {
                execution_session,
                open,
                ..
            } => {
                require(
                    request["session_id"].as_str() == Some(execution_session.session_id.as_str())
                        && request["task_id"].is_null()
                        && request["binding"].is_null()
                        && request["action"]["type"] != "command",
                    "conversation work changed its session or gained a Task",
                )?;
                match open {
                    ConversationOpenIntent::Opening { operation_id } => require(
                        request["action"]["type"] == "open"
                            && invocation.action == Action::ExecutionOpen
                            && invocation.operation_id == *operation_id,
                        "initial conversation work changed its Open operation",
                    ),
                    ConversationOpenIntent::Existing { .. } => require(
                        request["action"]["type"] != "open",
                        "existing conversation work cannot replace its initial Open",
                    ),
                }
            }
            Self::TaskSession {
                task,
                binding_generation,
                execution_session,
            } => {
                let binding: task_binding::SignedBinding =
                    serde_json::from_value(request["binding"].clone()).map_err(|_| {
                        AuthorizationError::InvalidProof("Task work has no binding")
                    })?;
                require(
                    binding.binding.task_ref == task.as_str()
                        && binding.binding.generation == *binding_generation
                        && binding.binding.session_id == execution_session.session_id
                        && request["action"]["type"] != "command",
                    "Task work changed its signed binding",
                )
            }
        }
    }

    pub fn validate_context(&self, prepared: &PrepareContext, target: &PublicKey) -> Result<()> {
        let context = &prepared.context;
        require(
            prepared.execution_target_public_key.as_ref() == Some(target)
                && context.conversation.as_ref().is_some_and(|v| !v.is_empty()),
            "execution work changed its conversation or target key",
        )?;
        match self {
            Self::TaskSession {
                task,
                binding_generation,
                execution_session,
            } => require(
                *binding_generation > 0
                    && prepared.binding_generation == Some(*binding_generation)
                    && context.task.as_ref() == Some(task)
                    && context.execution_session.as_ref() == Some(execution_session)
                    && execution_session.target == DeviceId::from_key(target)
                    && context
                        .executor_kind
                        .as_ref()
                        .is_some_and(|v| !v.is_empty()),
                "Task execution work changed its binding or session",
            ),
            Self::ConversationSession {
                conversation,
                execution_session,
                executor,
                canonical_cwd,
                open,
            } => {
                require(
                    prepared.binding_generation.is_none()
                        && context.task.is_none()
                        && context.conversation.as_ref() == Some(conversation)
                        && context.execution_session.as_ref() == Some(execution_session)
                        && execution_session.target == DeviceId::from_key(target)
                        && context.executor_kind.as_ref() == Some(executor)
                        && prepared.canonical_cwd.as_deref() == Some(canonical_cwd.as_str())
                        && !executor.is_empty()
                        && !canonical_cwd.is_empty(),
                    "conversation execution work changed its original Open",
                )?;
                let operation_id = match open {
                    ConversationOpenIntent::Opening { operation_id } => operation_id,
                    ConversationOpenIntent::Existing { open } => &open.operation_id,
                };
                require(
                    !operation_id.is_empty(),
                    "conversation Open has no operation",
                )
            }
            Self::Command { canonical_cwd, .. } => require(
                prepared.binding_generation.is_none()
                    && context.task.is_none()
                    && context.execution_session.is_none()
                    && context.executor_kind.is_none()
                    && prepared.canonical_cwd.as_deref() == Some(canonical_cwd.as_str())
                    && !canonical_cwd.is_empty(),
                "command execution work has a session or changed its cwd",
            ),
        }
    }

    pub fn matches_work(&self, work: &ExecutionWork) -> Result<()> {
        let matches = match (self, work) {
            (
                Self::ConversationSession {
                    conversation,
                    execution_session,
                    executor,
                    canonical_cwd,
                    open,
                },
                ExecutionWork::ConversationSession {
                    conversation: actual_conversation,
                    execution_session: actual_session,
                    executor: actual_executor,
                    canonical_cwd: actual_cwd,
                    open: actual_open,
                },
            ) => {
                conversation == actual_conversation
                    && execution_session == actual_session
                    && executor == actual_executor
                    && canonical_cwd == actual_cwd
                    && match open {
                        ConversationOpenIntent::Opening { operation_id } => {
                            operation_id == &actual_open.operation_id
                        }
                        ConversationOpenIntent::Existing { open } => open == actual_open,
                    }
            }
            _ => *self == work.intent(),
        };
        require(matches, "execution work differs from its approved intent")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProjectDispatchOrigin {
    TaskSession {
        task_creation: events::SignedEvent,
    },
    ConversationOpen,
    RemoteConversationSession {
        accepted_open: Box<execution_handoff::ExecutionHandoffResult>,
    },
    LocalConversationSession {
        accepted_open: Box<ProjectExecutionResult>,
    },
    Command,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProjectExecutionResult {
    pub dispatch: events::SignedEvent,
    pub receipt: Signed<ProjectExecutionReceipt>,
    pub result: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectExecutionReceipt {
    pub version: u16,
    pub dispatch: EventRef,
    pub project: ResourceRef,
    pub target_public_key: PublicKey,
    pub operation_id: String,
    pub original_invocation_hash: ObjectHash,
    pub original_admission_hash: ObjectHash,
    pub work: ExecutionWork,
    pub state: execution_handoff::ExecutionHandoffState,
    pub result_hash: ObjectHash,
    pub session: Option<execution_handoff::ExecutionSessionSnapshot>,
    pub issued_at_ms: u64,
}
impl sealed::Sealed for ProjectExecutionReceipt {}
impl SignedObject for ProjectExecutionReceipt {
    const DOMAIN: &'static str = "project-execution-receipt-v2";
}

enum DispatchOrigin<'a> {
    TaskSession(&'a events::SignedEvent),
    ConversationOpen,
    RemoteConversationSession(&'a execution_handoff::ExecutionHandoffResult),
    LocalConversationSession(&'a ProjectExecutionResult),
    Command,
}

impl<'a> From<&'a ProjectDispatchOrigin> for DispatchOrigin<'a> {
    fn from(origin: &'a ProjectDispatchOrigin) -> Self {
        match origin {
            ProjectDispatchOrigin::TaskSession { task_creation } => {
                Self::TaskSession(task_creation)
            }
            ProjectDispatchOrigin::ConversationOpen => Self::ConversationOpen,
            ProjectDispatchOrigin::RemoteConversationSession { accepted_open } => {
                Self::RemoteConversationSession(accepted_open)
            }
            ProjectDispatchOrigin::LocalConversationSession { accepted_open } => {
                Self::LocalConversationSession(accepted_open)
            }
            ProjectDispatchOrigin::Command => Self::Command,
        }
    }
}

/// Derives request-bound work. The caller verifies the dispatch admission and origin authority.
pub fn derive_project_dispatch_work(
    event: &events::Event,
    origin: &ProjectDispatchOrigin,
    target: &PublicKey,
) -> Result<ExecutionWork> {
    derive_dispatch_work(event, origin.into(), target)
}

pub(super) fn derive_remote_dispatch_work(
    event: &events::Event,
    origin: &execution_handoff::ExecutionOrigin,
    target: &PublicKey,
) -> Result<ExecutionWork> {
    use execution_handoff::ExecutionOrigin;
    let origin = match origin {
        ExecutionOrigin::TaskSession { task_creation } => {
            DispatchOrigin::TaskSession(task_creation)
        }
        ExecutionOrigin::ConversationOpen => DispatchOrigin::ConversationOpen,
        ExecutionOrigin::ConversationSession { accepted_open } => {
            DispatchOrigin::RemoteConversationSession(accepted_open)
        }
        ExecutionOrigin::Command => DispatchOrigin::Command,
    };
    derive_dispatch_work(event, origin, target)
}

pub(super) fn project_dispatch_request<'a>(
    event: &'a events::Event,
    target: &PublicKey,
) -> Result<(&'a events::AdmissionProof, &'a serde_json::Value)> {
    let events::EventCause::Invocation { admission } = &event.cause else {
        return Err(AuthorizationError::InvalidProof(
            "execution requires an admitted dispatch",
        ));
    };
    let events::EventBody::DispatchRecorded {
        operation_id,
        task_id,
        target_node,
        session_id,
        action,
    } = &event.body
    else {
        return Err(AuthorizationError::InvalidProof(
            "execution source is not a dispatch",
        ));
    };
    let source = &admission.source_body;
    let request = source
        .get("request")
        .ok_or(AuthorizationError::InvalidProof("missing original request"))?;
    let invocation = &admission.invocation.payload;
    require(
        source["method"] == "operate"
            && source["space_id"].as_str() == Some(event.network_id.as_str())
            && source["resource_id"].as_str() == Some(event.resource_id.as_str())
            && source["authorization_id"].as_str() == Some(operation_id.as_str())
            && invocation.operation_id == *operation_id
            && invocation.resource.space.as_str() == event.network_id
            && invocation.resource.resource == event.resource_id
            && invocation.body_hash == ObjectHash::body(source)?
            && invocation.action == execution_handoff::execution_action(request)?
            && event.blobs.is_empty()
            && request == action
            && request["space_id"].as_str() == Some(event.network_id.as_str())
            && request["conversation_id"].as_str() == Some(event.scope_id.as_str())
            && request["operation_id"].as_str() == Some(operation_id.as_str())
            && request["target_node"].as_str() == Some(target_node.as_str())
            && request["session_id"]
                == serde_json::to_value(session_id).map_err(invalid_dispatch)?
            && request["task_id"] == serde_json::to_value(task_id).map_err(invalid_dispatch)?
            && *target_node == node_fingerprint(target.as_str()).map_err(invalid_dispatch)?,
        "dispatch differs from the original signed request",
    )?;
    Ok((admission, request))
}

fn derive_dispatch_work(
    event: &events::Event,
    origin: DispatchOrigin<'_>,
    target: &PublicKey,
) -> Result<ExecutionWork> {
    use execution_handoff::ExecutionOrigin;
    let (admission, request) = project_dispatch_request(event, target)?;
    let events::EventBody::DispatchRecorded {
        operation_id,
        task_id,
        target_node,
        session_id,
        ..
    } = &event.body
    else {
        unreachable!()
    };
    let work = match origin {
        DispatchOrigin::TaskSession(task_creation) => {
            let binding: task_binding::SignedBinding =
                serde_json::from_value(request["binding"].clone()).map_err(invalid_dispatch)?;
            task_binding::validate_binding(&binding.binding).map_err(invalid_dispatch)?;
            let b = &binding.binding;
            require(
                b.network_id == event.network_id
                    && b.conversation_id == event.scope_id
                    && Some(b.task_id.as_str()) == task_id.as_deref()
                    && b.task_ref == task_creation.hash
                    && b.target_node == *target_node
                    && Some(b.session_id.as_str()) == session_id.as_deref()
                    && request["action"]["type"] != "command"
                    && (request["action"]["type"] != "open"
                        || (request["action"] == b.open_spec
                            && request["expected_revision"].as_u64() == Some(0))),
                "binding does not match its original operation",
            )?;
            ExecutionWork::TaskSession {
                task: EventRef::parse(&task_creation.hash)?,
                binding_hash: task_binding::binding_hash(b).map_err(invalid_dispatch)?,
                binding_generation: b.generation,
                execution_session: SessionRef {
                    target: DeviceId::from_key(target),
                    session_id: b.session_id.clone(),
                },
            }
        }
        DispatchOrigin::ConversationOpen => {
            require(
                task_id.is_none()
                    && request["binding"].is_null()
                    && request["action"]["type"] == "open"
                    && request["expected_revision"].as_u64() == Some(0),
                "conversation Open carries a Task, another action or a nonzero revision",
            )?;
            let session_id = session_id.as_ref().filter(|s| !s.is_empty()).ok_or(
                AuthorizationError::InvalidProof("conversation Open has no session"),
            )?;
            ExecutionWork::ConversationSession {
                conversation: event.scope_id.clone(),
                execution_session: SessionRef {
                    target: DeviceId::from_key(target),
                    session_id: session_id.clone(),
                },
                executor: dispatch_text(&request["action"]["executor"], 128)?,
                canonical_cwd: dispatch_text(&request["action"]["cwd"], 4096)?,
                open: ConversationOpenRef {
                    operation_id: operation_id.clone(),
                    invocation_hash: admission.invocation.hash()?,
                    admission_hash: ObjectHash::body(admission)?,
                },
            }
        }
        DispatchOrigin::RemoteConversationSession(accepted_open) => {
            require(
                matches!(
                    accepted_open.handoff.handoff.origin,
                    ExecutionOrigin::ConversationOpen
                ),
                "conversation source is not the first Open",
            )?;
            let (open_event, _, _, open_work) =
                execution_handoff::original_dispatch(&accepted_open.handoff.handoff)?;
            execution_handoff::verify_execution_handoff_receipt_signature(
                &accepted_open.handoff,
                &accepted_open.receipt,
                &accepted_open.result,
            )?;
            let remote = prepare::remote_selection(&admission.source_body)?.ok_or(
                AuthorizationError::InvalidProof("remote Open has no execution destination"),
            )?;
            require(
                remote.0 == accepted_open.handoff.handoff.execution_resource
                    && remote.1 == *target
                    && accepted_open.handoff.handoff.target_public_key == *target,
                "conversation operation changed its remote Open destination",
            )?;
            check_accepted_open(
                event,
                request,
                &open_event,
                &open_work,
                accepted_open.receipt.payload.state,
                accepted_open.receipt.payload.session.as_ref(),
                accepted_open.receipt.payload.issued_at_ms,
                accepted_open.handoff.invocation.payload.not_before_ms,
            )?;
            open_work
        }
        DispatchOrigin::LocalConversationSession(accepted_open) => {
            verify_project_execution_receipt_signature(
                &accepted_open.dispatch,
                &ProjectDispatchOrigin::ConversationOpen,
                &accepted_open.receipt,
                &accepted_open.result,
            )?;
            let open_event: events::Event =
                serde_json::from_str(&accepted_open.dispatch.canonical_body)
                    .map_err(invalid_dispatch)?;
            let open_work = &accepted_open.receipt.payload.work;
            require(
                prepare::remote_selection(&admission.source_body)?.is_none()
                    && admission.invocation.payload.audience.device == DeviceId::from_key(target)
                    && admission.invocation.payload.audience.service == ServiceId::from_key(target)
                    && accepted_open.receipt.payload.target_public_key == *target,
                "conversation operation changed its local Open destination",
            )?;
            let (open_admission, _) = project_dispatch_request(&open_event, target)?;
            check_accepted_open(
                event,
                request,
                &open_event,
                open_work,
                accepted_open.receipt.payload.state,
                accepted_open.receipt.payload.session.as_ref(),
                accepted_open.receipt.payload.issued_at_ms,
                open_admission.invocation.payload.not_before_ms,
            )?;
            open_work.clone()
        }
        DispatchOrigin::Command => {
            require(
                task_id.is_none()
                    && session_id.is_none()
                    && request["binding"].is_null()
                    && request["expected_revision"].as_u64() == Some(0)
                    && request["action"]["type"] == "command",
                "command has a Task, session, binding or revision",
            )?;
            dispatch_text(&request["action"]["command"], MAX_PROOF_BYTES)?;
            require(
                request["action"]["timeout_secs"]
                    .as_u64()
                    .is_some_and(|v| v > 0),
                "command timeout is invalid",
            )?;
            ExecutionWork::Command {
                request_hash: ObjectHash::body(request)?,
                canonical_cwd: dispatch_text(&request["action"]["cwd"], 4096)?,
            }
        }
    };
    let context = project_dispatch_context(event, &work, target)?;
    require(
        context.context == admission.invocation.payload.context,
        "dispatch work differs from its admitted context",
    )?;
    work.validate_context(&context, target)?;
    Ok(work)
}

#[allow(clippy::too_many_arguments)]
fn check_accepted_open(
    event: &events::Event,
    request: &serde_json::Value,
    open_event: &events::Event,
    open_work: &ExecutionWork,
    state: execution_handoff::ExecutionHandoffState,
    session: Option<&execution_handoff::ExecutionSessionSnapshot>,
    issued_at_ms: u64,
    not_before_ms: u64,
) -> Result<()> {
    let admission = match &event.cause {
        events::EventCause::Invocation { admission } => admission,
        _ => {
            return Err(AuthorizationError::InvalidProof(
                "continuation has no admission",
            ));
        }
    };
    let snapshot = session.ok_or(AuthorizationError::InvalidProof(
        "accepted Open has no created session",
    ))?;
    require(
        request["task_id"].is_null()
            && request["binding"].is_null()
            && request["action"]["type"] != "open"
            && request["action"]["type"] != "command"
            && matches!(state, execution_handoff::ExecutionHandoffState::Succeeded)
            && !matches!(
                snapshot.state.as_str(),
                "opening" | "unknown" | "resolved" | "failed" | "closed"
            )
            && issued_at_ms <= admission.admitted_at_ms
            && issued_at_ms >= not_before_ms
            && open_event.network_id == event.network_id
            && open_event.resource_id == event.resource_id
            && open_event.scope_id == event.scope_id
            && open_work.execution_session().map(|v| v.session_id.as_str())
                == request["session_id"].as_str(),
        "conversation operation changed its accepted Open or target",
    )
}

pub(super) fn project_dispatch_context(
    event: &events::Event,
    work: &ExecutionWork,
    target: &PublicKey,
) -> Result<PrepareContext> {
    let (admission, request) = project_dispatch_request(event, target)?;
    let (executor, cwd) = match work {
        ExecutionWork::TaskSession { .. } => {
            let binding: task_binding::SignedBinding =
                serde_json::from_value(request["binding"].clone()).map_err(invalid_dispatch)?;
            (
                Some(dispatch_text(&binding.binding.open_spec["executor"], 128)?),
                Some(dispatch_text(&binding.binding.open_spec["cwd"], 4096)?),
            )
        }
        ExecutionWork::ConversationSession {
            executor,
            canonical_cwd,
            ..
        } => (Some(executor.clone()), Some(canonical_cwd.clone())),
        ExecutionWork::Command { canonical_cwd, .. } => (None, Some(canonical_cwd.clone())),
    };
    Ok(PrepareContext {
        context: InvocationContext {
            conversation: Some(event.scope_id.clone()),
            task: work.task_ref().cloned(),
            sdk_session: admission.invocation.payload.context.sdk_session.clone(),
            execution_session: work.execution_session().cloned(),
            executor_kind: executor,
            workspace_resource: Some(event.resource_id.clone()),
        },
        binding_generation: work.binding_generation(),
        canonical_cwd: cwd,
        execution_target_public_key: Some(target.clone()),
    })
}

pub fn verify_project_execution_receipt_signature(
    dispatch: &events::SignedEvent,
    origin: &ProjectDispatchOrigin,
    receipt: &Signed<ProjectExecutionReceipt>,
    result: &serde_json::Value,
) -> Result<()> {
    receipt.verify_signature()?;
    let value = &receipt.payload;
    let verified = events::verify_signed_event(
        &serde_json::to_string(dispatch).map_err(invalid_dispatch)?,
        value.project.space.as_str(),
        value.project.resource.as_str(),
        value.target_public_key.as_str(),
    )
    .map_err(invalid_dispatch)?;
    let event = verified.event();
    let (admission, _) = project_dispatch_request(event, &value.target_public_key)?;
    admission.invocation.verify_signature()?;
    let work = derive_project_dispatch_work(event, origin, &value.target_public_key)?;
    require(
        canonical(receipt)?.len() <= MAX_PROOF_BYTES
            && receipt.issuer == value.target_public_key
            && value.version == AUTH_VERSION
            && value.dispatch.as_str() == dispatch.hash
            && admission.invocation.payload.audience.device == DeviceId::from_key(&receipt.issuer)
            && admission.invocation.payload.audience.service
                == ServiceId::from_key(&receipt.issuer)
            && prepare::remote_selection(&admission.source_body)?.is_none()
            && value.operation_id == admission.invocation.payload.operation_id
            && value.original_invocation_hash == admission.invocation.hash()?
            && value.original_admission_hash == ObjectHash::body(admission)?
            && value.work == work
            && value.result_hash == ObjectHash::body(result)?
            && value.issued_at_ms >= admission.admitted_at_ms,
        "Project execution receipt differs from its local dispatch",
    )?;
    if let Some(snapshot) = &value.session {
        let session = work
            .execution_session()
            .ok_or(AuthorizationError::InvalidProof(
                "command cannot carry a session snapshot",
            ))?;
        let context = project_dispatch_context(event, &work, &value.target_public_key)?;
        require(
            snapshot.original_project == value.project
                && snapshot.execution_resource == value.project
                && snapshot.work == work
                && snapshot.target == session.target
                && snapshot.session_id == session.session_id
                && Some(snapshot.executor.as_str()) == context.context.executor_kind.as_deref()
                && Some(snapshot.cwd.as_str()) == context.canonical_cwd.as_deref()
                && !snapshot.state.is_empty()
                && snapshot.state.len() <= 128
                && snapshot.expires_at_ms > value.issued_at_ms
                && snapshot.expires_at_ms - value.issued_at_ms <= EXECUTION_LEASE_MS,
            "local session snapshot differs from its Project work",
        )?;
    }
    Ok(())
}

fn invalid_dispatch(_: impl std::fmt::Display) -> AuthorizationError {
    AuthorizationError::InvalidProof("invalid Project dispatch evidence")
}

fn dispatch_text(value: &serde_json::Value, max: usize) -> Result<String> {
    value
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= max)
        .map(str::to_owned)
        .ok_or(AuthorizationError::InvalidProof(
            "invalid execution work text",
        ))
}
