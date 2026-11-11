#[path = "../src/host/profiles.rs"]
#[allow(dead_code)]
pub mod profiles;
mod host {
    pub use crate::profiles;
}
#[path = "../src/views/history.rs"]
pub mod history;
#[path = "../src/views/offline.rs"]
pub mod offline;
#[path = "../src/views/records.rs"]
pub mod records;
mod views {
    pub use crate::{history, records};
}
#[path = "../src/ui.rs"]
#[allow(dead_code)]
mod ui;
use serde_json::json;
use ui::{ComposerDraft, Conversation, decode_list};
use vibeface_protocol::workspace::{Attachment, InputIntent, InputTarget};

#[test]
fn native_input_rejects_empty_and_keeps_attachment_only_messages() {
    let mut draft = ComposerDraft {
        target: InputTarget::BotMain {
            bot_id: "bot-a".into(),
        },
        intent: InputIntent::Continue,
        text: " \n ".into(),
        attachments: vec![],
        expected_revision: None,
    };
    assert_eq!(draft.validate(), Err("请输入消息或添加附件".into()));
    draft.attachments.push(Attachment {
        hash: "a".repeat(64),
        size: 3,
        name: "note.txt".into(),
    });
    assert_eq!(draft.validate(), Ok(()));
}
#[test]
fn native_conversation_projection_keeps_markdown_stop_triggers_and_memory_cas() {
    let wire = json!({"messages":[{"message_id":"m1","role":"assistant","text":"## 原生\n\n`code`","event_hash":"e1","attachments":[{"hash":"abc","size":12,"name":"result.txt"}]}],"scheduler":{"jobs":[{"trigger":"input-7","state":"running","scope":{"kind":"main"}}]},"bot_settings":{"memory":[{"memory_id":"memory-1","text":"本人管理设备","metadata":{"epoch":1,"revision":"r2","control_revision":3,"source_event_hash":"e9","user_controlled":true}}]}});
    let conversation: Conversation = serde_json::from_value(wire).unwrap();
    assert_eq!(conversation.messages[0].text, "## 原生\n\n`code`");
    assert_eq!(conversation.messages[0].attachments[0].name, "result.txt");
    assert_eq!(conversation.scheduler.jobs[0].trigger, "input-7");
    assert_eq!(conversation.bot_settings.memory[0].metadata.revision, "r2");
    assert!(conversation.bot_settings.memory[0].metadata.user_controlled);
}
#[test]
fn absent_scheduler_is_not_an_active_reply() {
    let conversation: Conversation =
        serde_json::from_value(json!({"messages":[],"scheduler":null})).unwrap();
    assert!(conversation.scheduler.jobs.is_empty());
}
#[test]
fn malformed_wire_lists_do_not_look_like_a_synced_empty_workspace() {
    assert!(decode_list::<ui::Bot>(&json!({"unexpected":[]}), "bots").is_err());
    assert!(
        decode_list::<ui::Bot>(&json!({"bots":[{"name":"missing identities"}]}), "bots").is_err()
    );
    assert!(
        decode_list::<ui::Bot>(&json!({"bots":[]}), "bots")
            .unwrap()
            .is_empty()
    );
}
#[test]
fn every_native_input_target_serializes_exact_routing_identity() {
    let targets = [
        InputTarget::BotMain { bot_id: "b".into() },
        InputTarget::Task {
            task_id: "t".into(),
        },
        InputTarget::Work {
            work_id: "w".into(),
        },
        InputTarget::Routine {
            bot_id: "b".into(),
            trigger: "r".into(),
        },
        InputTarget::ChannelMember {
            channel_id: "c".into(),
            bot_id: "b".into(),
        },
        InputTarget::ChannelTask {
            channel_id: "c".into(),
            task_id: "t".into(),
            bot_id: "b".into(),
        },
    ];
    let actual = targets
        .into_iter()
        .map(|target| serde_json::to_value(target).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            json!({"kind":"bot_main","bot_id":"b"}),
            json!({"kind":"task","task_id":"t"}),
            json!({"kind":"work","work_id":"w"}),
            json!({"kind":"routine","bot_id":"b","trigger":"r"}),
            json!({"kind":"channel_member","channel_id":"c","bot_id":"b"}),
            json!({"kind":"channel_task","channel_id":"c","task_id":"t","bot_id":"b"})
        ]
    );
}

#[test]
fn conversation_reads_reject_cross_conversation_identity_and_scope() {
    assert!(
        ui::parse_conversation(
            json!({"conversation":{"conversation_id":"other","owner_epoch":1}}),
            "expected",
            None
        )
        .is_err()
    );
    assert!(ui::parse_conversation(json!({"conversation":{"conversation_id":"expected","owner_epoch":1},"scheduler":{"scope_jobs":[{"conversation_id":"other","trigger":"t","scope":{"kind":"main"}}]}}),"expected",None).is_err());
    let parsed=ui::parse_conversation(json!({"conversation":{"conversation_id":"expected","owner_epoch":1},"messages":[{"message_id":"a","role":"user","text":"main"},{"message_id":"b","role":"user","text":"task","target":{"kind":"task","task_id":"task-1"}}],"scheduler":{"jobs":[{"trigger":"main","state":"running","scope":{"kind":"main"}},{"trigger":"work","state":"running","scope":{"kind":"task","work_id":"work-1"}}]}}),"expected",None).unwrap();
    assert_eq!(
        parsed
            .messages
            .iter()
            .map(|m| m.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
    assert_eq!(
        parsed
            .scheduler
            .jobs
            .iter()
            .map(|j| j.trigger.as_str())
            .collect::<Vec<_>>(),
        vec!["main"]
    );
}
#[test]
fn progress_ignores_duplicates_and_does_not_append_after_a_sequence_gap() {
    let mut streams = vec![];
    let event = |sequence, kind: &str, text: &str| json!({"type":"manager_progress","conversation_id":"c","source_node":"node","trigger":"message:m","invocation_id":"i","sequence":sequence,"progress":{"type":kind,"text":text}});
    assert!(!ui::apply_progress(&mut streams, &event(1, "text_snapshot", "Hello")).unwrap());
    assert!(!ui::apply_progress(&mut streams, &event(1, "text_delta", " duplicate")).unwrap());
    assert_eq!(streams[0].text, "Hello");
    assert!(ui::apply_progress(&mut streams, &event(3, "text_delta", " gap")).unwrap());
    assert_eq!(streams[0].text, "Hello");
    assert!(!ui::apply_progress(&mut streams, &event(4, "text_snapshot", "Hello world")).unwrap());
    assert_eq!(streams[0].text, "Hello world");
}
#[test]
fn target_conversation_follows_exact_task_owner() {
    let snapshot = ui::Snapshot {
        bots: vec![
            ui::Bot {
                bot_id: "bot-a".into(),
                conversation_id: "chat-a".into(),
                ..Default::default()
            },
            ui::Bot {
                bot_id: "bot-b".into(),
                conversation_id: "chat-b".into(),
                ..Default::default()
            },
        ],
        tasks: vec![ui::Task {
            task_id: "task-b".into(),
            owner_bot: "bot-b".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        ui::target_conversation(
            &InputTarget::Task {
                task_id: "task-b".into()
            },
            &snapshot,
            Some("chat-a")
        )
        .unwrap(),
        "chat-b"
    );
    assert!(
        ui::target_conversation(
            &InputTarget::Task {
                task_id: "missing".into()
            },
            &snapshot,
            Some("chat-a")
        )
        .is_err()
    );
}
#[test]
fn offline_projection_is_bounded_and_never_grants_live_write_authority() {
    let scope = offline::CacheScope {
        profile_id: "profile-a".into(),
        network_id: "network-a".into(),
        client_id: "client-a".into(),
        target_node: "node-a".into(),
    };
    let mut snapshot = ui::Snapshot {
        online: true,
        synced: true,
        network: Some(scope.network_id.clone()),
        client_id: Some(scope.client_id.clone()),
        write_state: "writable".into(),
        bots: vec![ui::Bot {
            bot_id: "bot-a".into(),
            conversation_id: "chat-a".into(),
            name: "Alpha".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    snapshot.conversation = std::sync::Arc::new(ui::Conversation {
        conversation: Some(ui::ConversationIdentity {
            conversation_id: "chat-a".into(),
            owner: Some("node-a".into()),
            owner_epoch: 1,
        }),
        messages: (0..128)
            .map(|index| ui::Message {
                message_id: format!("message-{index}"),
                role: "assistant".into(),
                text: "x".repeat(16384),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    });
    let bytes = offline::encode(&snapshot, &scope, Some("chat-a")).unwrap();
    assert!(bytes.len() <= offline::MAX_CACHE_BYTES);
    let cached = offline::decode(&bytes, &scope)
        .unwrap()
        .into_snapshot(9, true);
    assert!(cached.cached);
    assert!(!cached.online);
    assert!(!cached.synced);
    assert!(!cached.writable);
    assert!(cached.can_save_locally);
    assert!(cached.cache_omitted > 0);
    assert_eq!(
        cached.conversation.messages.last().unwrap().message_id,
        "message-127"
    );
    for wrong in [
        offline::CacheScope {
            profile_id: "other".into(),
            ..scope.clone()
        },
        offline::CacheScope {
            network_id: "other".into(),
            ..scope.clone()
        },
        offline::CacheScope {
            client_id: "other".into(),
            ..scope.clone()
        },
        offline::CacheScope {
            target_node: "other".into(),
            ..scope.clone()
        },
    ] {
        assert!(offline::decode(&bytes, &wrong).is_err());
    }
    let mut corrupt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    corrupt["provider_credentials"] = json!({"forbidden":"fixture-only"});
    assert!(offline::decode(&serde_json::to_vec(&corrupt).unwrap(), &scope).is_err());
}
#[test]
fn explicit_manual_unlock_keeps_identity_sealed_without_a_vault_daemon() {
    let directory = std::env::temp_dir().join(format!(
        "vibeface-native-unlock-fixture-{}",
        uuid::Uuid::new_v4()
    ));
    let password = "controlled-device-passphrase";
    let key = vibeface_client::ClientKey::create(&directory, password).unwrap();
    let id = key.device_id().to_owned();
    drop(key);
    let bytes = std::fs::read(directory.join("client.json")).unwrap();
    let file: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(file.get("envelope").is_some());
    assert!(!String::from_utf8(bytes).unwrap().contains(password));
    assert!(vibeface_client::ClientKey::open(&directory, "wrong-password").is_err());
    assert_eq!(
        vibeface_client::ClientKey::open(&directory, password)
            .unwrap()
            .device_id(),
        id
    );
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn explicit_node_selection_is_not_replaced_by_the_remembered_node() {
    let remembered = vibeface_client::Entry {
        origin: "https://gateway-old.invalid".into(),
        node_id: "a".repeat(64),
    };
    let target = ui::connection_entry("", &"b".repeat(64), Some(&remembered), true).unwrap();
    assert_eq!(target.origin, "https://gateway-old.invalid");
    assert_eq!(target.node_id, "b".repeat(64));
    let route =
        ui::connection_entry("https://gateway-new.invalid", "", Some(&remembered), true).unwrap();
    assert_eq!(route.origin, "https://gateway-new.invalid");
    assert_eq!(route.node_id, "a".repeat(64));
    assert!(
        ui::connection_entry(
            "https://user:password@gateway.invalid",
            &"a".repeat(64),
            None,
            true
        )
        .is_err()
    );
}

#[test]
fn cached_history_never_reuses_live_page_cursors_or_claims_complete_coverage() {
    use vibeface_protocol::history::HistoryCoverage;
    let scope = offline::CacheScope {
        profile_id: "default".into(),
        network_id: "network".into(),
        client_id: "client".into(),
        target_node: "node".into(),
    };
    let bot = ui::Bot {
        bot_id: "bot".into(),
        conversation_id: "conversation".into(),
        ..Default::default()
    };
    let snapshot = ui::Snapshot {
        online: true,
        synced: true,
        network: Some(scope.network_id.clone()),
        client_id: Some(scope.client_id.clone()),
        write_state: "writable".into(),
        bots: vec![bot],
        conversation: std::sync::Arc::new(ui::Conversation {
            conversation: Some(ui::ConversationIdentity {
                conversation_id: "conversation".into(),
                ..Default::default()
            }),
            history: std::collections::BTreeMap::from([
                (
                    "messages".into(),
                    HistoryCoverage {
                        next_cursor: Some("live-cursor".into()),
                        complete: false,
                    },
                ),
                (
                    "works".into(),
                    HistoryCoverage {
                        next_cursor: None,
                        complete: true,
                    },
                ),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let bytes = offline::encode(&snapshot, &scope, Some("conversation")).unwrap();
    let cold = offline::decode(&bytes, &scope)
        .unwrap()
        .into_snapshot(2, true);
    assert!(cold.conversation.history.is_empty());
    assert!(cold.cached);
    assert!(!cold.online);
    assert!(!cold.writable);
}

#[test]
fn reachable_owner_jobs_take_precedence_without_duplicating_triggers() {
    let scheduler:ui::Scheduler=serde_json::from_value(json!({"jobs":[{"trigger":"same","state":"running","scope":{"kind":"main"}}],"remote":{"owner":"owner","reachable":true,"jobs":[{"trigger":"same","state":"completed","scope":{"kind":"main"}}]}})).unwrap();
    let jobs = scheduler.effective_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, "completed");
    let mut streams = vec![ui::LiveProgress {
        trigger: Some("same".into()),
        ..Default::default()
    }];
    ui::prune_terminal_progress(
        &mut streams,
        &ui::Conversation {
            scheduler,
            ..Default::default()
        },
    );
    assert!(streams.is_empty());
}

#[test]
fn answered_and_closed_main_questions_remain_available_in_history() {
    let conversation=ui::parse_conversation(json!({"conversation":{"conversation_id":"chat","owner_epoch":1},"questions":[{"question_id":"closed","conversation_id":"chat","target":{"kind":"bot_main","bot_id":"bot"},"closed":"resolved","text":"Original question"}]}),"chat",Some("bot")).unwrap();
    assert_eq!(conversation.questions.len(), 1);
    assert_eq!(conversation.questions[0].text, "Original question");
    assert_eq!(
        conversation.questions[0].closed.as_deref(),
        Some("resolved")
    );
}

#[test]
fn persisted_preferred_routes_keep_the_exact_node_and_do_not_follow_an_explicit_new_target() {
    use vibeface_client::{Endpoint, Entry};
    let original = Entry {
        origin: "https://gateway.example".into(),
        node_id: "a".repeat(64),
    };
    let remembered = Endpoint::Preferred {
        gateway: original.clone(),
        addresses: vec!["127.0.0.1:41501".into()],
    };
    let route = ui::connection_endpoint("", "", "", "", Some(&remembered), None).unwrap();
    assert!(
        matches!(route,Endpoint::Preferred{gateway,addresses}if gateway==original&&addresses==vec!["127.0.0.1:41501"])
    );
    let route =
        ui::connection_endpoint("", "", &"b".repeat(64), "", Some(&remembered), None).unwrap();
    assert!(
        matches!(route,Endpoint::Gateway(entry)if entry.node_id=="b".repeat(64)&&entry.origin==original.origin)
    );
    assert!(
        ui::connection_endpoint("lan", "", &"b".repeat(64), "", Some(&remembered), None).is_err(),
        "old Node direct hints must not be reused for a newly selected Node"
    );
    let route = ui::connection_endpoint(
        "auto",
        "https://gateway.example",
        &"a".repeat(64),
        "[::1]:41501",
        None,
        None,
    )
    .unwrap();
    assert!(matches!(route,Endpoint::Preferred{addresses,..}if addresses==vec!["[::1]:41501"]));
    assert!(
        ui::connection_endpoint(
            "lan",
            "",
            &"a".repeat(64),
            "hostname.local:41501",
            None,
            None
        )
        .is_err()
    );
}

#[test]
fn retries_require_explicit_server_proof_and_unknown_execution_never_looks_stopped() {
    let job = |value| serde_json::from_value::<ui::Job>(value).unwrap();
    assert!(
        !job(json!({"trigger":"message:a","state":"failed","scope":{"kind":"main"}})).can_retry()
    );
    assert!(
        job(
            json!({"trigger":"message:a","state":"failed","retryable":true,"scope":{"kind":"main"}})
        )
        .can_retry()
    );
    assert!(!job(json!({"trigger":"message:a","state":"running","retryable":true,"scope":{"kind":"main"}})).can_retry());
    assert!(!job(json!({"trigger":"message:a","state":"interrupted","retryable":true,"scope":{"kind":"main"},"failure":{"kind":"unknown","detail":"server result unknown"}})).can_retry());
    for state in [
        "completed",
        "cancelled",
        "failed",
        "interrupted",
        "answered",
        "steered",
        "unknown",
    ] {
        assert!(
            !job(json!({"trigger":"message:a","state":state,"scope":{"kind":"main"}})).is_active()
        );
    }
    assert!(
        job(json!({"trigger":"message:a","state":"running","scope":{"kind":"main"}})).is_active()
    );
}

#[test]
fn unreachable_or_unloaded_owner_jobs_cannot_authorize_retry_from_local_cache() {
    let mut scheduler:ui::Scheduler=serde_json::from_value(json!({"jobs":[{"trigger":"message:original","message_id":"original","state":"failed","retryable":true,"scope":{"kind":"main"}}],"remote":{"owner":"real-owner","reachable":false,"jobs":[{"trigger":"message:original","message_id":"original","state":"failed","retryable":true,"scope":{"kind":"main"}}]}})).unwrap();
    assert!(
        scheduler.retry_job_for_message("original").is_none(),
        "unreachable owner cannot be replaced by a retryable local observation"
    );
    scheduler.remote.as_mut().unwrap().reachable = true;
    scheduler.remote.as_mut().unwrap().jobs.clear();
    assert!(
        scheduler.retry_job_for_message("original").is_none(),
        "an unloaded owner row cannot borrow the local proof"
    );
    scheduler.remote.as_mut().unwrap().jobs=serde_json::from_value(json!([{"trigger":"message:original","message_id":"original","state":"running","scope":{"kind":"main"}}])).unwrap();
    assert!(
        !scheduler
            .retry_job_for_message("original")
            .unwrap()
            .can_retry()
    );
    scheduler.remote = None;
    assert!(
        scheduler
            .retry_job_for_message("original")
            .unwrap()
            .can_retry(),
        "local ownership keeps the explicit server proof"
    );
}
