#![cfg(feature = "native-ui-tests")]
extern crate gpui_kit as gpui;
#[path = "../src/app.rs"]
#[allow(dead_code, unused_imports)]
mod app;
#[path = "../src/host/mod.rs"]
#[allow(dead_code, unused_imports)]
mod host;
#[path = "../src/theme.rs"]
#[allow(dead_code)]
mod theme;
#[path = "../src/ui.rs"]
#[allow(dead_code)]
mod ui;
#[path = "../src/views/mod.rs"]
#[allow(dead_code, unused_imports)]
mod views;
#[path = "../src/widgets/mod.rs"]
#[allow(dead_code)]
mod widgets;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext, WindowOptions};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use ui::{Bot, Conversation, ConversationIdentity, Route, Snapshot};
use vibeface_protocol::workspace::{InputIntent, InputTarget};
use views::backend::{Command, Event};

struct Harness {
    window: AnyWindowHandle,
    view: Entity<app::Desktop>,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    _snapshots: watch::Sender<Arc<Snapshot>>,
}
fn fixture() -> Snapshot {
    Snapshot {
        generation: 7,
        online: true,
        synced: true,
        writable: true,
        can_save_locally: true,
        network: Some("network-fixture".into()),
        client_id: Some("client-fixture".into()),
        bots: vec![
            Bot {
                bot_id: "bot-a".into(),
                conversation_id: "chat-a".into(),
                name: "Alpha".into(),
                ..Default::default()
            },
            Bot {
                bot_id: "bot-b".into(),
                conversation_id: "chat-b".into(),
                name: "Beta".into(),
                ..Default::default()
            },
        ],
        conversation: Arc::new(Conversation {
            conversation: Some(ConversationIdentity {
                conversation_id: "chat-a".into(),
                owner: Some("node-a".into()),
                owner_epoch: 1,
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}
fn open(cx: &mut TestAppContext, route: Route, snapshot: Snapshot) -> Harness {
    cx.update(gpui_kit::init);
    let (tx, commands) = mpsc::channel(32);
    let (events, event_rx) = mpsc::channel(32);
    let (snapshots, snapshot_rx) = watch::channel(Arc::new(snapshot.clone()));
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            cx.new(|cx| app::Desktop::new(tx, event_rx, snapshot_rx, window, cx))
        })
        .unwrap()
    });
    view.update(cx, |view, cx| {
        view.snapshot = Arc::new(snapshot);
        view.route = route;
        view.selected = Some("chat-a".into());
        view.prepare_history_list();
        view.attachment_scope = Some("chat-a".into());
        cx.notify();
    });
    Harness {
        window,
        view,
        commands,
        events,
        _snapshots: snapshots,
    }
}
fn enter_message(h: &Harness, cx: &mut TestAppContext, text: &str) {
    h.window
        .update(cx, |_, window, cx| {
            h.view.update(cx, |view, cx| {
                view.composer
                    .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx))
            })
        })
        .unwrap();
}
fn click(h: &Harness, cx: &mut TestAppContext, id: &str) {
    let id = id.to_owned();
    h.window
        .update(cx, |_, window, cx| {
            window.render_frame(cx);
            window.click(gpui_kit::SharedString::from(id), cx);
        })
        .unwrap();
    cx.run_until_parked();
}
fn scoped(command: Command) -> (u64, Command) {
    match command {
        Command::Scoped {
            generation,
            command,
        } => (generation, *command),
        _ => panic!("network action was not fenced by its rendered generation"),
    }
}

#[gpui_kit::test]
fn explicit_task_selection_routes_to_its_owner_not_the_open_bot(cx: &mut TestAppContext) {
    let mut snapshot = fixture();
    snapshot.tasks = vec![ui::Task {
        task_id: "task-b".into(),
        owner_bot: "bot-b".into(),
        title: "Beta task".into(),
        revision: 9,
        ..Default::default()
    }];
    let mut h = open(cx, Route::Chat("chat-a".into()), snapshot);
    h.view.update(cx, |view, cx| {
        view.target_kind = 1;
        cx.notify();
    });
    enter_message(&h, cx, "针对 Beta 的补充");
    click(&h, cx, "target-choice-0");
    click(&h, cx, "send");
    let (generation, command) = scoped(h.commands.try_recv().expect("input command"));
    assert_eq!(generation, 7);
    match command {
        Command::Input {
            conversation_id,
            draft,
            owner,
        } => {
            assert!(owner.is_some(), "a composer must carry its UI owner");
            assert_eq!(conversation_id, "chat-b");
            assert_eq!(
                draft.target,
                InputTarget::Task {
                    task_id: "task-b".into()
                }
            );
            assert_eq!(draft.expected_revision, Some(9));
            assert_eq!(draft.intent, InputIntent::Continue);
            assert_eq!(draft.text, "针对 Beta 的补充");
        }
        _ => panic!("wrong native action"),
    }
    assert_eq!(
        h.view
            .read_with(cx, |view, cx| view.composer.read(cx).value().to_string()),
        "针对 Beta 的补充",
        "editor must retain input until durable storage acknowledges it"
    );
}
#[gpui_kit::test]
fn an_unselected_non_main_target_does_not_submit(cx: &mut TestAppContext) {
    let mut h = open(cx, Route::Chat("chat-a".into()), fixture());
    h.view.update(cx, |view, cx| {
        view.target_kind = 1;
        view.selected_target = None;
        cx.notify();
    });
    enter_message(&h, cx, "不要发给第一个任务");
    click(&h, cx, "send");
    assert!(h.commands.try_recv().is_err());
    assert_eq!(
        h.view
            .read_with(cx, |view, _| view.notice.clone())
            .as_deref(),
        Some("请明确选择消息目标。")
    );
}
#[gpui_kit::test]
fn offline_members_can_save_but_editor_clears_only_after_durable_ack(cx: &mut TestAppContext) {
    let mut snapshot = fixture();
    snapshot.online = false;
    snapshot.writable = false;
    snapshot.can_save_locally = true;
    let mut h = open(cx, Route::Chat("chat-a".into()), snapshot);
    enter_message(&h, cx, "离线待发内容");
    click(&h, cx, "send");
    let (_, command) = scoped(h.commands.try_recv().expect("offline staging command"));
    let ack = match command {
        Command::Input {
            conversation_id,
            draft,
            owner,
        } => {
            assert_eq!(conversation_id, "chat-a");
            assert_eq!(draft.text, "离线待发内容");
            ui::InputAck {
                owner,
                conversation_id,
                draft,
            }
        }
        _ => panic!("wrong offline native action"),
    };
    assert_eq!(
        h.view
            .read_with(cx, |view, cx| view.composer.read(cx).value().to_string()),
        "离线待发内容"
    );
    assert!(h.events.try_send(Event::InputStored(ack)).is_ok());
    cx.run_until_parked();
    assert_eq!(
        h.view
            .read_with(cx, |view, cx| view.composer.read(cx).value().to_string()),
        ""
    );
}
#[gpui_kit::test]
fn local_read_only_session_cannot_stage_chat(cx: &mut TestAppContext) {
    let mut snapshot = fixture();
    snapshot.local_session = true;
    snapshot.writable = false;
    snapshot.can_save_locally = false;
    let mut h = open(cx, Route::Chat("chat-a".into()), snapshot);
    enter_message(&h, cx, "本机会话无成员签名权限");
    click(&h, cx, "send");
    assert!(h.commands.try_recv().is_err());
    assert_eq!(
        h.view
            .read_with(cx, |view, cx| view.composer.read(cx).value().to_string()),
        "本机会话无成员签名权限"
    );
}
#[gpui_kit::test]
fn pending_choice_keeps_the_question_conversation(cx: &mut TestAppContext) {
    let mut snapshot = fixture();
    snapshot.questions = vec![ui::Question {
        question_id: "question-b".into(),
        conversation_id: "chat-b".into(),
        text: "允许本次操作？".into(),
        choices: vec![ui::Choice {
            label: "仅允许一次".into(),
        }],
        ..Default::default()
    }];
    let mut h = open(cx, Route::Tasks, snapshot);
    click(&h, cx, "q-question-b-仅允许一次");
    let (generation, command) = scoped(h.commands.try_recv().expect("answer command"));
    assert_eq!(generation, 7);
    match command {
        Command::Answer {
            conversation_id,
            question_id,
            text,
        } => {
            assert_eq!(conversation_id, "chat-b");
            assert_eq!(question_id, "question-b");
            assert_eq!(text, "仅允许一次");
        }
        _ => panic!("wrong answer action"),
    }
}

#[gpui_kit::test]
fn verified_local_node_operator_can_open_and_save_settings_without_network_write(
    cx: &mut TestAppContext,
) {
    let mut snapshot = fixture();
    snapshot.local_session = true;
    snapshot.writable = false;
    snapshot.can_save_locally = false;
    snapshot.can_configure = true;
    let mut h = open(cx, Route::Devices, snapshot);
    click(&h, cx, "node-settings-editor");
    let (_, command) = scoped(h.commands.try_recv().expect("read node config"));
    assert!(matches!(
        command,
        Command::EditSettings(views::backend::SettingsKind::Node)
    ));
    assert!(
        h.events
            .try_send(Event::EditForm {
                editor: views::forms::Editor::NodeSettings,
                values: std::collections::BTreeMap::from([
                    ("roots", "/controlled/project".into()),
                    ("provider", String::new()),
                    ("backup_automatic", "false".into())
                ])
            })
            .is_ok()
    );
    cx.run_until_parked();
    click(&h, cx, "save-form");
    let (generation, command) = scoped(h.commands.try_recv().expect("save local node config"));
    assert_eq!(generation, 7);
    match command {
        Command::Save { editor, values, .. } => {
            assert!(matches!(editor, views::forms::Editor::NodeSettings));
            assert_eq!(
                values.get("roots").map(String::as_str),
                Some("/controlled/project")
            );
            assert_eq!(values.get("provider").map(String::as_str), Some(""));
        }
        _ => panic!("wrong local settings action"),
    }
    assert!(!h.view.read_with(cx, |view, _| view.snapshot.writable));
    assert!(!h.view.read_with(cx, |view, _| view.snapshot.can_manage));
}
#[gpui_kit::test]
fn non_operator_member_cannot_open_node_configuration(cx: &mut TestAppContext) {
    let mut snapshot = fixture();
    snapshot.can_configure = false;
    let mut h = open(cx, Route::Devices, snapshot);
    click(&h, cx, "node-settings-editor");
    assert!(h.commands.try_recv().is_err());
    assert!(h.view.read_with(cx, |view, _| view.form.is_none()));
}
#[gpui_kit::test]
fn profile_switch_requires_its_explicit_confirmation(cx: &mut TestAppContext) {
    use host::profiles::{PersonalProfile, ProfileHints, ProfileId};
    let mut snapshot = fixture();
    let id = "00000000-0000-4000-8000-000000000001";
    snapshot.profile_id = Some("default".into());
    snapshot.profiles = vec![
        PersonalProfile {
            id: ProfileId::parse("default").unwrap(),
            label: "原网络".into(),
            hints: ProfileHints::default(),
        },
        PersonalProfile {
            id: ProfileId::parse(id).unwrap(),
            label: "另一个网络".into(),
            hints: ProfileHints::default(),
        },
    ];
    let mut h = open(cx, Route::Connect, snapshot);
    click(&h, cx, &format!("switch-profile-{id}"));
    assert!(h.commands.try_recv().is_err());
    assert!(h.view.read_with(cx, |view, _| view.confirmation.is_some()));
    click(&h, cx, "confirm-danger");
    match h.commands.try_recv().expect("explicit profile switch") {
        Command::SwitchProfile(profile) => assert_eq!(profile, id),
        _ => panic!("profile switch must not submit a network command"),
    }
}
#[gpui_kit::test]
fn cold_cached_member_can_stage_without_claiming_a_fresh_read(cx: &mut TestAppContext) {
    let mut snapshot = fixture();
    snapshot.online = false;
    snapshot.synced = false;
    snapshot.writable = false;
    snapshot.cached = true;
    snapshot.can_save_locally = true;
    snapshot.status = "离线缓存，状态尚未重新确认".into();
    let mut h = open(cx, Route::Chat("chat-a".into()), snapshot);
    enter_message(&h, cx, "冷启动后的本机待发输入");
    click(&h, cx, "send");
    let (_, command) = scoped(h.commands.try_recv().expect("cached target staging"));
    assert!(matches!(command, Command::Input { .. }));
    assert!(!h.view.read_with(cx, |view, _| view.snapshot.synced));
    assert!(!h.view.read_with(cx, |view, _| view.snapshot.writable));
}
#[gpui_kit::test]
fn explicit_manual_unlock_uses_the_entered_passphrase_without_requesting_a_vault(
    cx: &mut TestAppContext,
) {
    let mut h = open(cx, Route::Connect, fixture());
    h.window
        .update(cx, |_, window, cx| {
            h.view.update(cx, |view, cx| {
                view.edit(
                    views::forms::Editor::Connect,
                    std::collections::BTreeMap::from([
                        ("origin", "https://fixture.invalid".into()),
                        ("node_id", "a".repeat(64)),
                        ("unlock_mode", "manual".into()),
                        ("unlock_password", "fixture-unlock-passphrase".into()),
                    ]),
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    click(&h, cx, "save-form");
    let command = h.commands.try_recv().expect("manual unlock intent");
    match command {
        Command::Save { values, editor, .. } => {
            assert!(matches!(editor, views::forms::Editor::Connect));
            assert_eq!(
                values.get("passphrase").map(String::as_str),
                Some("fixture-unlock-passphrase")
            );
            assert!(!values.contains_key("unlock_password"));
        }
        _ => panic!("manual unlock must use the explicit identity path"),
    }
}
#[gpui_kit::test]
fn stale_critical_events_cannot_clear_a_newer_draft_or_copy_an_old_profile_secret(
    cx: &mut TestAppContext,
) {
    let h = open(cx, Route::Chat("chat-a".into()), fixture());
    enter_message(&h, cx, "当前档案尚未保存的文字");
    assert!(
        h.events
            .try_send(Event::Scoped {
                generation: 6,
                profile_id: None,
                event: Box::new(Event::InputStored(ui::InputAck {
                    owner: None,
                    conversation_id: "chat-a".into(),
                    draft: ui::ComposerDraft {
                        target: InputTarget::BotMain {
                            bot_id: "bot-a".into()
                        },
                        intent: InputIntent::Continue,
                        text: "当前档案尚未保存的文字".into(),
                        attachments: vec![],
                        expected_revision: None
                    }
                }))
            })
            .is_ok()
    );
    assert!(
        h.events
            .try_send(Event::Scoped {
                generation: 7,
                profile_id: Some("other-profile".into()),
                event: Box::new(Event::CopySecret(zeroize::Zeroizing::new(
                    "fixture-old-profile-secret".into()
                )))
            })
            .is_ok()
    );
    cx.run_until_parked();
    assert_eq!(
        h.view
            .read_with(cx, |view, cx| view.composer.read(cx).value().to_string()),
        "当前档案尚未保存的文字"
    );
    assert!(cx.update(|cx| cx.read_from_clipboard()).is_none());
}
#[gpui_kit::test]
fn a_verified_aligning_projection_can_stage_locally_without_granting_online_writes(
    cx: &mut TestAppContext,
) {
    let mut snapshot = fixture();
    snapshot.synced = false;
    snapshot.writable = false;
    snapshot.cached = false;
    snapshot.projection_valid = true;
    snapshot.can_save_locally = true;
    snapshot.write_state = "aligning".into();
    let mut h = open(cx, Route::Chat("chat-a".into()), snapshot);
    enter_message(&h, cx, "等待状态补齐的输入");
    click(&h, cx, "send");
    let (_, command) = scoped(h.commands.try_recv().expect("pure local staging"));
    assert!(matches!(command, Command::Input { .. }));
    assert!(!h.view.read_with(cx, |view, _| view.snapshot.writable));
}

#[gpui_kit::test]
fn native_history_reads_the_selected_section_with_current_scope(cx: &mut TestAppContext) {
    use vibeface_protocol::history::{HistoryCoverage, HistorySection};
    let mut snapshot = fixture();
    snapshot.conversation = Arc::new(Conversation {
        history: std::collections::BTreeMap::from([(
            "works".into(),
            HistoryCoverage {
                next_cursor: Some("opaque-older".into()),
                complete: false,
            },
        )]),
        ..(*snapshot.conversation).clone()
    });
    let mut h = open(cx, Route::History(HistorySection::Works), snapshot);
    click(&h, cx, "load-history-works");
    let (generation, command) = scoped(h.commands.try_recv().expect("native history page command"));
    assert_eq!(generation, 7);
    match command {
        Command::LoadHistory {
            conversation_id,
            section,
        } => {
            assert_eq!(conversation_id, "chat-a");
            assert_eq!(section, HistorySection::Works);
        }
        _ => panic!("history control routed to another action"),
    }
}

#[gpui_kit::test]
fn offline_history_keeps_coverage_without_requesting_a_page(cx: &mut TestAppContext) {
    use vibeface_protocol::history::{HistoryCoverage, HistorySection};
    let mut snapshot = fixture();
    snapshot.online = false;
    snapshot.cached = true;
    snapshot.conversation = Arc::new(Conversation {
        history: std::collections::BTreeMap::from([(
            "memory".into(),
            HistoryCoverage {
                next_cursor: Some("old-cursor".into()),
                complete: false,
            },
        )]),
        ..(*snapshot.conversation).clone()
    });
    let mut h = open(cx, Route::History(HistorySection::Memory), snapshot);
    click(&h, cx, "load-history-memory");
    assert!(
        h.commands.try_recv().is_err(),
        "offline history button must not issue a page request"
    );
}

#[gpui_kit::test]
fn overwritten_profile_restore_uses_the_original_change(cx: &mut TestAppContext) {
    use vibeface_protocol::history::HistorySection;
    let original = serde_json::json!({"change_id":"my-change","conversation_id":"chat-a","cause":null,"event_hash":"event-a","created_at_ms":10,"change":{"kind":"profile","name":{"before":"Before","after":"My Alpha"}}});
    let record = serde_json::json!({"change_id":"my-change","by":"other-device","field":"name","value":"Their Alpha"});
    let mut snapshot = fixture();
    snapshot.conversation = Arc::new(Conversation {
        changes: vec![serde_json::from_value(original).unwrap()],
        overwritten: vec![serde_json::from_value(record).unwrap()],
        ..(*snapshot.conversation).clone()
    });
    let mut h = open(cx, Route::History(HistorySection::Overwritten), snapshot);
    click(&h, cx, "restore-profile-0");
    assert!(
        h.commands.try_recv().is_err(),
        "restoration requires the visible confirmation"
    );
    click(&h, cx, "confirm-danger");
    let (_, command) = scoped(h.commands.try_recv().expect("confirmed restoration"));
    match command {
        Command::RestoreProfile {
            conversation_id,
            record,
        } => {
            assert_eq!(conversation_id, "chat-a");
            assert_eq!(record.change_id, "my-change");
        }
        _ => panic!("wrong restore command"),
    }
}

#[gpui_kit::test]
fn prepending_messages_and_updating_rows_preserve_the_reader_anchor(cx: &mut TestAppContext) {
    let rows = (0..20)
        .map(|index| ui::Message {
            message_id: format!("message-{index}"),
            role: "user".into(),
            text: format!("正文 {index}"),
            ..Default::default()
        })
        .collect::<Vec<_>>();
    let h = open(cx, Route::Chat("chat-a".into()), fixture());
    h.view.update(cx, |view, cx| {
        view.timeline.replace(rows.clone(), cx);
        view.timeline.list.scroll_to(gpui_kit::ListOffset {
            item_ix: 8,
            offset_in_item: gpui_kit::px(17.),
        });
        let mut older = vec![ui::Message {
            message_id: "older".into(),
            role: "user".into(),
            text: "更早记录".into(),
            ..Default::default()
        }];
        older.extend(rows);
        view.timeline.replace(older.clone(), cx);
        assert_eq!(
            view.timeline.list.logical_scroll_top(),
            gpui_kit::ListOffset {
                item_ix: 9,
                offset_in_item: gpui_kit::px(17.)
            }
        );
        assert_eq!(view.timeline.message(9).unwrap().message_id, "message-8");
        older[9].text.push_str("，刷新后的内容");
        view.timeline.replace(older, cx);
        assert_eq!(
            view.timeline.list.logical_scroll_top(),
            gpui_kit::ListOffset {
                item_ix: 9,
                offset_in_item: gpui_kit::px(17.)
            }
        );
    });
}

#[gpui_kit::test]
fn native_non_message_history_preserves_record_identity_and_offset(cx: &mut TestAppContext) {
    let h = open(
        cx,
        Route::History(vibeface_protocol::history::HistorySection::Works),
        fixture(),
    );
    h.view.update(cx, |view, _| {
        let rows = (0..1000)
            .map(|index| format!("work-{index}"))
            .collect::<Vec<_>>();
        view.history_list
            .replace("same-network-generation".into(), rows.clone());
        view.history_list.list.scroll_to(gpui_kit::ListOffset {
            item_ix: 400,
            offset_in_item: gpui_kit::px(23.),
        });
        let mut older = vec!["older-work".into()];
        older.extend(rows);
        view.history_list
            .replace("same-network-generation".into(), older.clone());
        assert_eq!(
            view.history_list.list.logical_scroll_top(),
            gpui_kit::ListOffset {
                item_ix: 401,
                offset_in_item: gpui_kit::px(23.)
            }
        );
        view.history_list.replace("new-generation".into(), older);
        assert_eq!(
            view.history_list.list.logical_scroll_top(),
            gpui_kit::ListOffset {
                item_ix: 0,
                offset_in_item: gpui_kit::px(0.)
            }
        );
    });
}

#[gpui_kit::test]
fn native_skill_history_reads_and_copies_the_complete_body(cx: &mut TestAppContext) {
    let mut snapshot = fixture();
    snapshot.conversation=Arc::new(Conversation{bot_settings:ui::BotSettings{skills:vec![serde_json::from_value(serde_json::json!({"skill_id":"full-skill","name":"完整技能","description":"摘要","body":"# 完整指令\n\n保留正文，不以摘要代替。"})).unwrap()],..Default::default()},..(*snapshot.conversation).clone()});
    let h = open(
        cx,
        Route::History(vibeface_protocol::history::HistorySection::Skills),
        snapshot,
    );
    click(&h, cx, "copy-skill-body-full-skill");
    let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(
        copied.as_deref(),
        Some("# 完整指令\n\n保留正文，不以摘要代替。")
    );
}

#[gpui_kit::test]
fn join_activation_keeps_the_existing_form_and_opens_only_one_invitation(cx: &mut TestAppContext) {
    let mut h = open(cx, Route::Home, fixture());
    h.window
        .update(cx, |_, window, cx| {
            h.view.update(cx, |view, cx| {
                assert!(view.accept_join_activation(
                    "first-controlled-invitation".into(),
                    window,
                    cx
                ));
                let original = view.form.as_ref().unwrap().submission_id.clone();
                assert!(!view.accept_join_activation(
                    "second-controlled-invitation".into(),
                    window,
                    cx
                ));
                assert_eq!(view.form.as_ref().unwrap().submission_id, original);
                assert_eq!(
                    view.form
                        .as_ref()
                        .unwrap()
                        .values(cx)
                        .get("ticket")
                        .map(String::as_str),
                    Some("first-controlled-invitation")
                );
            })
        })
        .unwrap();
    assert!(
        h.commands.try_recv().is_err(),
        "opening an invitation only prefills its form"
    );
}

#[gpui_kit::test]
fn join_activation_waits_for_active_network_operations_and_user_confirmation(
    cx: &mut TestAppContext,
) {
    let h = open(cx, Route::Home, fixture());
    h.window
        .update(cx, |_, window, cx| {
            h.view.update(cx, |view, cx| {
                view.snapshot = Arc::new(Snapshot {
                    identity_busy: true,
                    ..(*view.snapshot).clone()
                });
                assert!(!view.accept_join_activation("controlled-invitation".into(), window, cx));
                assert!(view.form.is_none());
                view.snapshot = Arc::new(Snapshot {
                    identity_busy: false,
                    ..(*view.snapshot).clone()
                });
                view.confirmation = Some(Command::SwitchProfile("profile-next".into()));
                assert!(!view.accept_join_activation("controlled-invitation".into(), window, cx));
                assert!(view.form.is_none());
                assert!(matches!(view.confirmation, Some(Command::SwitchProfile(_))));
                view.confirmation = None;
                assert!(view.accept_join_activation("controlled-invitation".into(), window, cx));
            })
        })
        .unwrap();
}

fn edit_message(h: &Harness, cx: &mut TestAppContext, text: &str) {
    h.window
        .update(cx, |_, window, cx| {
            h.view.update(cx, |view, cx| {
                view.composer.update(cx, |input, cx| {
                    input.replace_all(text.to_owned(), window, cx)
                })
            })
        })
        .unwrap();
    cx.run_until_parked();
}
fn next_composer_ack(h: &mut Harness) -> ui::InputAck {
    loop {
        let (_, command) = scoped(h.commands.try_recv().expect("composer command"));
        if let Command::Input {
            conversation_id,
            draft,
            owner,
        } = command
        {
            assert!(
                owner.is_some(),
                "actual composer producers must carry ownership"
            );
            return ui::InputAck {
                owner,
                conversation_id,
                draft,
            };
        }
    }
}
fn acknowledge(h: &Harness, cx: &mut TestAppContext, ack: ui::InputAck) {
    let owner = ack.owner.as_ref().expect("composer ack owner");
    h.events
        .try_send(Event::Scoped {
            generation: owner.generation,
            profile_id: owner.profile_id.clone(),
            event: Box::new(Event::InputStored(ack)),
        })
        .unwrap();
    cx.run_until_parked();
}
fn fill_commands(h: &Harness, cx: &mut TestAppContext) {
    h.view.update(cx, |view, cx| {
        for _ in 0..32 {
            view.send(Command::WatchRecord(views::backend::ActiveRecord::None), cx);
        }
    });
}
fn assert_text(h: &Harness, cx: &TestAppContext, expected: &str) {
    assert_eq!(
        h.view
            .read_with(cx, |view, cx| view.composer.read(cx).value().to_string()),
        expected
    );
}

#[gpui_kit::test]
fn stored_input_keeps_text_when_target_changes_or_text_changes_back(cx: &mut TestAppContext) {
    let mut snapshot = fixture();
    snapshot.tasks = vec![ui::Task {
        task_id: "task-b".into(),
        owner_bot: "bot-b".into(),
        title: "Beta task".into(),
        revision: 9,
        ..Default::default()
    }];
    let mut h = open(cx, Route::Chat("chat-a".into()), snapshot);
    enter_message(&h, cx, "original text");
    click(&h, cx, "send");
    let ack = next_composer_ack(&mut h);
    click(&h, cx, "target-1");
    click(&h, cx, "target-choice-0");
    acknowledge(&h, cx, ack);
    assert_text(&h, cx, "original text");
    assert!(!h.view.read_with(cx, |view, _| view.sending));
    enter_message(&h, cx, "same text");
    click(&h, cx, "send");
    let ack = next_composer_ack(&mut h);
    assert_eq!(ack.conversation_id, "chat-b");
    edit_message(&h, cx, "edited text");
    edit_message(&h, cx, "same text");
    acknowledge(&h, cx, ack);
    assert_text(&h, cx, "same text");
}

#[gpui_kit::test]
fn stored_input_keeps_new_attachments_and_another_conversation_draft(cx: &mut TestAppContext) {
    let mut h = open(cx, Route::Chat("chat-a".into()), fixture());
    enter_message(&h, cx, "retained text");
    click(&h, cx, "send");
    let ack = next_composer_ack(&mut h);
    let attachment = vibeface_protocol::workspace::Attachment {
        hash: "a".repeat(64),
        size: 4,
        name: "new-file.txt".into(),
    };
    h.events
        .try_send(Event::Attachments {
            conversation_id: "chat-a".into(),
            attachments: vec![attachment.clone()],
        })
        .unwrap();
    cx.run_until_parked();
    acknowledge(&h, cx, ack);
    assert_text(&h, cx, "retained text");
    assert_eq!(
        h.view.read_with(cx, |view, _| view.attachments.clone()),
        vec![attachment]
    );
    click(&h, cx, "send");
    let ack = next_composer_ack(&mut h);
    h.view.update(cx, |view, cx| {
        view.navigate(Route::Chat("chat-b".into()), cx)
    });
    acknowledge(&h, cx, ack);
    assert_text(&h, cx, "retained text");
}

#[gpui_kit::test]
fn new_generation_releases_old_busy_without_clearing_new_editor(cx: &mut TestAppContext) {
    let mut h = open(cx, Route::Chat("chat-a".into()), fixture());
    enter_message(&h, cx, "keep across scope");
    click(&h, cx, "send");
    let old = next_composer_ack(&mut h);
    let mut snapshot = fixture();
    snapshot.generation = 8;
    snapshot.profile_id = Some("profile-b".into());
    snapshot.network = Some("network-b".into());
    snapshot.client_id = Some("client-b".into());
    h._snapshots.send_replace(Arc::new(snapshot));
    cx.run_until_parked();
    assert!(!h.view.read_with(cx, |view, _| view.sending));
    assert_text(&h, cx, "keep across scope");
    h.view.update(cx, |view, cx| {
        view.navigate(Route::Chat("chat-a".into()), cx)
    });
    let attachment = vibeface_protocol::workspace::Attachment {
        hash: "b".repeat(64),
        size: 2,
        name: "new-scope.txt".into(),
    };
    h.view.update(cx, |view, cx| {
        view.attachments = vec![attachment.clone()];
        cx.notify();
    });
    click(&h, cx, "send");
    let new = next_composer_ack(&mut h);
    assert_ne!(
        old.owner.as_ref().unwrap().token,
        new.owner.as_ref().unwrap().token
    );
    acknowledge(&h, cx, old);
    assert_text(&h, cx, "keep across scope");
    assert!(h.view.read_with(cx, |view, _| view.sending));
    assert_eq!(
        h.view.read_with(cx, |view, _| view.attachments.clone()),
        vec![attachment]
    );
    acknowledge(&h, cx, new);
    assert_text(&h, cx, "");
    assert!(!h.view.read_with(cx, |view, _| view.sending));
}

#[gpui_kit::test]
fn read_errors_and_unowned_inputs_do_not_release_an_owned_submission(cx: &mut TestAppContext) {
    let mut h = open(cx, Route::Chat("chat-a".into()), fixture());
    enter_message(&h, cx, "owned text");
    click(&h, cx, "send");
    let ack = next_composer_ack(&mut h);
    h.events
        .try_send(Event::Error("controlled history read error".into()))
        .unwrap();
    let mut unowned = ack.clone();
    unowned.owner = None;
    h.events.try_send(Event::InputStored(unowned)).unwrap();
    cx.run_until_parked();
    assert!(h.view.read_with(cx, |view, _| view.sending));
    assert_text(&h, cx, "owned text");
    let mut wrong = ack.owner.clone().unwrap();
    wrong.token = "other-submission".into();
    h.events
        .try_send(Event::InputFailed {
            owner: wrong,
            error: "old failure".into(),
        })
        .unwrap();
    cx.run_until_parked();
    assert!(h.view.read_with(cx, |view, _| view.sending));
    h.events
        .try_send(Event::InputFailed {
            owner: ack.owner.unwrap(),
            error: "controlled local save failure".into(),
        })
        .unwrap();
    cx.run_until_parked();
    assert!(!h.view.read_with(cx, |view, _| view.sending));
    assert_text(&h, cx, "owned text");
}

#[gpui_kit::test]
fn record_composer_returns_owner_and_clears_only_its_unchanged_draft(cx: &mut TestAppContext) {
    let mut snapshot = fixture();
    snapshot.tasks = vec![ui::Task {
        task_id: "task-b".into(),
        owner_bot: "bot-b".into(),
        title: "Beta task".into(),
        revision: 9,
        ..Default::default()
    }];
    let mut h = open(cx, Route::Task("task-b".into()), snapshot);
    cx.simulate_window_resize(
        h.window,
        gpui_kit::size(gpui_kit::px(1100.), gpui_kit::px(1800.)),
    );
    enter_message(&h, cx, "record task input");
    click(&h, cx, "record-send");
    let ack = next_composer_ack(&mut h);
    assert_eq!(ack.conversation_id, "chat-b");
    assert_eq!(
        ack.draft.target,
        InputTarget::Task {
            task_id: "task-b".into()
        }
    );
    assert_eq!(ack.draft.expected_revision, Some(9));
    acknowledge(&h, cx, ack);
    assert_text(&h, cx, "");
}

#[gpui_kit::test]
fn full_command_queue_keeps_composer_retryable(cx: &mut TestAppContext) {
    let mut h = open(cx, Route::Chat("chat-a".into()), fixture());
    enter_message(&h, cx, "queue must preserve me");
    fill_commands(&h, cx);
    click(&h, cx, "send");
    assert!(!h.view.read_with(cx, |view, _| view.sending));
    assert_text(&h, cx, "queue must preserve me");
    let mut count = 0;
    while let Ok(command) = h.commands.try_recv() {
        let (_, command) = scoped(command);
        assert!(matches!(command, Command::WatchRecord(_)));
        count += 1;
    }
    assert_eq!(count, 32);
    click(&h, cx, "send");
    let ack = next_composer_ack(&mut h);
    acknowledge(&h, cx, ack);
    assert_text(&h, cx, "");
}

#[gpui_kit::test]
fn full_queue_preserves_manual_identity_and_ordinary_form_fields(cx: &mut TestAppContext) {
    let h = open(cx, Route::Join, fixture());
    fill_commands(&h, cx);
    h.window
        .update(cx, |_, window, cx| {
            h.view.update(cx, |view, cx| {
                view.edit(
                    views::forms::Editor::Join,
                    std::collections::BTreeMap::from([
                        ("unlock_mode", "manual".into()),
                        ("unlock_password", "controlled password".into()),
                        ("ticket", "controlled invitation".into()),
                    ]),
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    click(&h, cx, "save-form");
    assert!(!h.view.read_with(cx, |view, _| view.form_pending));
    let values = h
        .view
        .read_with(cx, |view, cx| view.form.as_ref().unwrap().values(cx));
    assert_eq!(values["unlock_password"], "controlled password");
    assert_eq!(values["ticket"], "controlled invitation");
    h.window
        .update(cx, |_, window, cx| {
            h.view.update(cx, |view, cx| {
                view.edit(
                    views::forms::Editor::Answer("question-a".into(), "chat-a".into()),
                    std::collections::BTreeMap::from([("text", "ordinary answer".into())]),
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    click(&h, cx, "save-form");
    assert!(!h.view.read_with(cx, |view, _| view.form_pending));
    assert_eq!(
        h.view
            .read_with(cx, |view, cx| view.form.as_ref().unwrap().values(cx))["text"],
        "ordinary answer"
    );
}

#[gpui_kit::test]
fn full_queue_preserves_form_when_vault_completion_arrives(cx: &mut TestAppContext) {
    let h = open(cx, Route::Join, fixture());
    fill_commands(&h, cx);
    h.window
        .update(cx, |_, window, cx| {
            h.view.update(cx, |view, cx| {
                view.edit(
                    views::forms::Editor::Recovery,
                    std::collections::BTreeMap::from([
                        ("unlock_mode", "vault".into()),
                        ("ticket", "vault invitation".into()),
                        ("password", "recovery password".into()),
                    ]),
                    window,
                    cx,
                );
                view.form_pending = true;
                let values = view.form.as_ref().unwrap().values(cx);
                assert!(!view.finish_credential_save(
                    Command::Save {
                        editor: views::forms::Editor::Recovery,
                        values,
                        conversation_id: None
                    },
                    Ok("controlled vault credential".into()),
                    window,
                    cx
                ));
            })
        })
        .unwrap();
    assert!(!h.view.read_with(cx, |view, _| view.form_pending));
    assert_eq!(
        h.view
            .read_with(cx, |view, cx| view.form.as_ref().unwrap().values(cx))["password"],
        "recovery password"
    );
}

#[gpui_kit::test]
fn same_arc_cached_chat_survives_navigation_away_and_back(cx: &mut TestAppContext) {
    let mut snapshot = fixture();
    snapshot.online = false;
    snapshot.cached = true;
    snapshot.conversation = Arc::new(Conversation {
        messages: vec![ui::Message {
            message_id: "cached-message".into(),
            role: "assistant".into(),
            text: "retained offline history".into(),
            ..Default::default()
        }],
        ..(*snapshot.conversation).clone()
    });
    let original = snapshot.conversation.clone();
    let h = open(cx, Route::Chat("chat-a".into()), snapshot);
    h.view.update(cx, |view, cx| {
        view.timeline
            .replace(view.snapshot.conversation.messages.clone(), cx);
        view.navigate(Route::Home, cx);
        view.navigate(Route::Chat("chat-a".into()), cx);
        assert!(Arc::ptr_eq(&view.snapshot.conversation, &original));
        assert_eq!(
            view.timeline.message(0).unwrap().text,
            "retained offline history"
        );
    });
}

#[gpui_kit::test]
fn long_live_row_keeps_its_identity_and_intra_row_offset(cx: &mut TestAppContext) {
    let h = open(cx, Route::Chat("chat-a".into()), fixture());
    h.view.update(cx, |view, cx| {
        let mut live = ui::LiveProgress {
            key: "manager:chat-a:node:trigger".into(),
            conversation_id: "chat-a".into(),
            text: "long live line\n".repeat(200),
            status: "running".into(),
            tools: vec![ui::ToolProgress {
                item_id: "tool-a".into(),
                name: "tool".into(),
                status: "running".into(),
                text: "first tool".into(),
                sequence: 1,
            }],
            ..Default::default()
        };
        view.timeline.replace(
            vec![ui::Message {
                message_id: "base".into(),
                role: "user".into(),
                text: "base message".into(),
                ..Default::default()
            }],
            cx,
        );
        view.timeline
            .update_live(&[live.clone()], Some("chat-a"), cx);
        let stable = view.timeline.message(2).unwrap().message_id.clone();
        view.timeline.list.scroll_to(gpui_kit::ListOffset {
            item_ix: 2,
            offset_in_item: gpui_kit::px(117.),
        });
        live.text.push_str("new delta");
        view.timeline
            .update_live(&[live.clone()], Some("chat-a"), cx);
        assert_eq!(
            view.timeline.list.logical_scroll_top(),
            gpui_kit::ListOffset {
                item_ix: 2,
                offset_in_item: gpui_kit::px(117.)
            }
        );
        assert_eq!(view.timeline.message(2).unwrap().message_id, stable);
        live.tools.push(ui::ToolProgress {
            item_id: "tool-b".into(),
            name: "tool".into(),
            status: "running".into(),
            text: "second tool".into(),
            sequence: 2,
        });
        view.timeline.update_live(&[live], Some("chat-a"), cx);
        assert_eq!(
            view.timeline.list.logical_scroll_top(),
            gpui_kit::ListOffset {
                item_ix: 3,
                offset_in_item: gpui_kit::px(117.)
            }
        );
        assert_eq!(view.timeline.message(3).unwrap().message_id, stable);
    });
}

#[gpui_kit::test]
fn connected_form_waits_for_actual_workspace_before_opening_business(cx: &mut TestAppContext) {
    let mut snapshot = fixture();
    snapshot.online = false;
    snapshot.synced = false;
    let h = open(cx, Route::Connect, snapshot.clone());
    let id = h
        .window
        .update(cx, |_, window, cx| {
            h.view.update(cx, |view, cx| {
                view.edit(
                    views::forms::Editor::Connect,
                    std::collections::BTreeMap::new(),
                    window,
                    cx,
                );
                view.form.as_ref().unwrap().submission_id.clone()
            })
        })
        .unwrap();
    h.events.try_send(Event::FormSaved(id)).unwrap();
    cx.run_until_parked();
    assert!(matches!(
        h.view.read_with(cx, |view, _| view.route.clone()),
        Route::Connect
    ));
    snapshot.online = true;
    snapshot.synced = true;
    h._snapshots.send_replace(Arc::new(snapshot));
    cx.run_until_parked();
    assert!(matches!(
        h.view.read_with(cx, |view, _| view.route.clone()),
        Route::Home
    ));
}

mod prepaid_delivery {
    use super::{app, ui, views};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        AnyWindowHandle, AppContext, Entity, Role, ScrollDelta, TestAppContext, WindowOptions,
        point, px,
    };
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::{mpsc, watch};
    use ui::*;
    use views::backend::{Command, Event};
    fn order(digit: char, role: &str) -> TransferOrder {
        serde_json::from_value(json!({"order_id":digit.to_string().repeat(64),"intent_id":format!("00000000-0000-4000-8000-00000000000{}",if digit=='d'{1}else{2}),"role":role,"mode":"paid","acceptance":"recoverable","source_peer":"a".repeat(64),"receiver_peer":"b".repeat(64),"provider_peer":"c".repeat(64),"name":if role=="provider"{None}else{Some("file.bin")},"bytes":4,"sha256":if role=="provider"{None}else{Some("f".repeat(64))},"phase":if role=="sender"{"prepared"}else{"funded"},"evidence":"none","windows":[{"index":0,"bytes":4,"committed_bytes":0,"state":"pending","decryption_proven":false}],"deadlines":{"quote_expires_at":2000000000,"hold_deadline":2000000100,"key_deadline":2000000200,"open_deadline":2000000300,"retain_until":2000000400,"priority_deadline":2000000150},"payment":{"chain_id":"1","asset":format!("0x{}","3".repeat(40)),"payer":format!("0x{}","5".repeat(40)),"provider_wallet":format!("0x{}","4".repeat(40)),"budget_id":"2".repeat(64),"state":if role=="sender"{"prepared"}else{"confirmed"},"delivery_fee":"5","reservation_fee":"2","priority_fee":"3","total":"10","gas_cap":"7","earned":"0","withdrawn":"0","refunded":"0","settlement":null}})).unwrap()
    }
    fn budget() -> TransferBudget {
        serde_json::from_value(json!({"budget_id":"2".repeat(64),"payer":format!("0x{}","5".repeat(40)),"payee":format!("0x{}","4".repeat(40)),"provider_peer":"c".repeat(64),"asset":format!("0x{}","3".repeat(40)),"deposit":"100","free":"100","locked":"0","earned":"0","withdrawn":"0","refunded":"0","state":"open","gas_cap":"7","ready_windows":[]})).unwrap()
    }
    fn snapshot(role: &str) -> Snapshot {
        let node = match role {
            "receiver" => "b",
            "provider" => "c",
            _ => "a",
        }
        .repeat(64);
        Snapshot {
            generation: 7,
            online: true,
            synced: true,
            writable: true,
            can_manage: true,
            network: Some("controlled-network".into()),
            client_id: Some("controlled-client".into()),
            delivery: Delivery {
                node_peer: node,
                wallet: Wallet {
                    state: "locked".into(),
                    address: Some(if role == "provider" {
                        budget().payee
                    } else {
                        budget().payer
                    }),
                    reason: None,
                },
                enabled: true,
                charity_enabled: true,
                paid_enabled: true,
                max_file_bytes: 1048576,
                budgets: vec![budget()],
                orders: vec![order('d', role), order('e', role)],
                providers: vec![TransferProvider {
                    device_id: "c".repeat(64),
                    paid: true,
                    offer: Some(TransferOffer {
                        price: "5".into(),
                        settlement: Some(TransferOfferSettlement {
                            chain_id: 1,
                            escrow: format!("0x{}", "6".repeat(40)),
                            asset: budget().asset,
                        }),
                    }),
                }],
                ..Default::default()
            },
            ..Default::default()
        }
    }
    struct Harness {
        window: AnyWindowHandle,
        view: Entity<app::Desktop>,
        commands: mpsc::Receiver<Command>,
        _events: mpsc::Sender<Event>,
        _snapshots: watch::Sender<Arc<Snapshot>>,
    }
    fn open(cx: &mut TestAppContext, snapshot: Snapshot) -> Harness {
        cx.update(gpui_kit::init);
        let (tx, commands) = mpsc::channel(32);
        let (events, rx) = mpsc::channel(32);
        let (snapshots, watch) = watch::channel(Arc::new(snapshot.clone()));
        let (window, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
                cx.new(|cx| app::Desktop::new(tx, rx, watch, window, cx))
            })
            .unwrap()
        });
        view.update(cx, |view, cx| {
            view.snapshot = Arc::new(snapshot);
            view.route = Route::Transfers;
            cx.notify();
        });
        Harness {
            window,
            view,
            commands,
            _events: events,
            _snapshots: snapshots,
        }
    }
    fn click(h: &Harness, cx: &mut TestAppContext, id: &str) {
        let id = gpui_kit::SharedString::from(id.to_owned());
        h.window
            .update(cx, |_, window, cx| {
                window.render_frame(cx);
                let target = window.find(id.clone());
                if !target.visible() {
                    assert!(
                        target.path().contains(&"page-scroll".into()),
                        "offscreen financial control is outside the page scroller: {id}"
                    );
                    let delta = px(350.) - target.bounds().center().y;
                    let viewport = window.viewport_size();
                    let mut page = window.within("page-scroll");
                    let anchor = page
                        .elements()
                        .into_iter()
                        .find(|element| {
                            let center = element.bounds().center();
                            element.visible()
                                && element.role() == Some(Role::Button)
                                && center.x > px(0.)
                                && center.x < viewport.width
                                && center.y > px(0.)
                                && center.y < viewport.height
                        })
                        .expect("visible page control to receive the native wheel event");
                    page.scroll(
                        anchor.path().last().unwrap().clone(),
                        ScrollDelta::Pixels(point(px(0.), delta)),
                        cx,
                    );
                    window.render_frame(cx);
                }
                window.click(id, cx);
            })
            .unwrap();
        cx.run_until_parked();
    }
    fn command(h: &mut Harness) -> Command {
        match h.commands.try_recv().expect("explicit financial command") {
            Command::Scoped {
                generation,
                command,
            } => {
                assert_eq!(generation, 7);
                *command
            }
            _ => panic!("financial command lost its rendered scope"),
        }
    }

    fn assert_original_order_texts(window: &gpui_kit::Window, sha256: &str) {
        let review = window.find("delivery-financial-review");
        assert_eq!(review.role(), Some(Role::Group));
        assert!(review.visible());
        let texts: Vec<_> = gpui_kit::base::test_support::texts(window)
            .into_iter()
            .filter(|text| text.path().starts_with(review.path()) && !text.is_alternative())
            .map(|text| text.text().to_owned())
            .collect();
        for (digit, intent) in [('d', 1), ('e', 2)] {
            assert!(texts.contains(&format!(
                "原订单 {}\n原意图 00000000-0000-4000-8000-00000000000{intent}\n4 字节 · 摘要 {sha256}",
                digit.to_string().repeat(64)
            )));
        }
        assert_eq!(
            texts
                .iter()
                .filter(|text| text.starts_with("原服务费 "))
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["原服务费 10 · 1个片段"; 2]
        );
        assert_eq!(
            texts
                .iter()
                .filter(|text| text.starts_with("原截止时间"))
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![
                "原截止时间（Unix秒）：报价 2000000000 · 持有 2000000100 · 密钥 2000000200 · 打开 2000000300 · 保留 2000000400 · 时效 2000000150";
                2
            ]
        );
    }

    #[gpui_kit::test]
    fn two_original_orders_are_reviewed_before_batch_confirmation(cx: &mut TestAppContext) {
        let mut h = open(cx, snapshot("sender"));
        click(&h, cx, &format!("confirm-batch-{}", "2".repeat(64)));
        assert!(h.commands.try_recv().is_err());
        h.view.update(cx, |view, cx| {
            let mut next = (*view.snapshot).clone();
            next.delivery.orders[0].payment.delivery_fee = "94".into();
            next.delivery.orders[0].payment.total = "99".into();
            next.delivery.orders[0].payment.gas_cap = "88".into();
            next.delivery.orders[0]
                .deadlines
                .as_mut()
                .unwrap()
                .quote_expires_at = 2000000999;
            view.snapshot = Arc::new(next);
            cx.notify();
        });
        h.window
            .update(cx, |_, window, cx| {
                window.render_frame(cx);
                assert_original_order_texts(window, &"f".repeat(64));
                assert_eq!(
                    window.find("delivery-financial-review").label(),
                    Some(
                        format!(
                            "两笔原订单 {} 和 {} · 预算 {} · 服务费合计 20",
                            "d".repeat(64),
                            "e".repeat(64),
                            "2".repeat(64)
                        )
                        .as_str()
                    )
                );
                assert_eq!(
                    window
                        .within("delivery-financial-review")
                        .find(gpui_kit::SharedString::from(format!(
                            "delivery-order-fee-{}",
                            "d".repeat(64)
                        )))
                        .label(),
                    Some("原付款方累计链费上限 7")
                );
            })
            .unwrap();
        click(&h, cx, "confirm-danger");
        match command(&mut h) {
            Command::DeliveryConfirmBatch(orders) => {
                assert_eq!(orders[0].order_id, "d".repeat(64));
                assert_eq!(orders[1].order_id, "e".repeat(64));
                assert_eq!(orders[0].payment.total, "10");
                assert_eq!(orders[1].payment.total, "10");
                assert_eq!(orders[0].payment.gas_cap, "7");
                assert_eq!(
                    orders[0].deadlines.as_ref().unwrap().quote_expires_at,
                    2000000000
                );
            }
            _ => panic!("not the reviewed pair"),
        }
    }
    #[gpui_kit::test]
    fn receiver_batch_and_provider_flush_require_distinct_explicit_confirmation(
        cx: &mut TestAppContext,
    ) {
        let mut h = open(cx, snapshot("receiver"));
        click(&h, cx, &format!("accept-batch-{}", "2".repeat(64)));
        assert!(h.commands.try_recv().is_err());
        h.window
            .update(cx, |_, window, cx| {
                window.render_frame(cx);
                assert_original_order_texts(window, &"f".repeat(64));
            })
            .unwrap();
        click(&h, cx, "confirm-danger");
        assert!(matches!(command(&mut h), Command::DeliveryAcceptBatch(_)));
        let mut state = snapshot("provider");
        state.delivery.budgets[0].gas_cap = "0".into();
        for order in &mut state.delivery.orders {
            order.payment.gas_cap = "0".into();
        }
        state.delivery.budgets[0].ready_windows = vec![
            TransferReadyWindow {
                order_id: "d".repeat(64),
                index: 0,
            },
            TransferReadyWindow {
                order_id: "e".repeat(64),
                index: 0,
            },
        ];
        let mut h = open(cx, state);
        click(&h, cx, &format!("flush-budget-{}", "2".repeat(64)));
        assert!(h.commands.try_recv().is_err());
        h.window.update(cx,|_,window,cx| {
            window.render_frame(cx);
            assert_original_order_texts(window, "未提供");
            let review=window.within("delivery-financial-review");
            assert_eq!(review.find("delivery-payer-chain-fee-cap").label(),Some("本机记录的付款方累计链费上限 0"));
            assert_eq!(review.find("delivery-provider-spending-note").label(),Some("中转设备的链费支出沿用本机既有授权、限额和公开规则；付款方的金额不是中转设备的支出限额。"));
            assert_eq!(review.find(gpui_kit::SharedString::from(format!("delivery-order-fee-{}","d".repeat(64)))).label(),Some("此订单视图不提供中转设备的本机链费授权"));
        }).unwrap();
        click(&h, cx, "confirm-danger");
        match command(&mut h) {
            Command::DeliveryFlushBudget(review) => {
                assert_eq!(review.budget.ready_windows.len(), 2);
                assert_eq!(review.orders.len(), 2);
                assert_eq!(review.budget.budget_id, "2".repeat(64));
                assert!(
                    vibeface_client::delivery_flush_batch_id(&review.budget.batch_binding())
                        .is_ok()
                );
            }
            _ => panic!("not the original budget review"),
        }
    }
    #[gpui_kit::test]
    fn paid_form_uses_existing_immutable_budget_and_exposes_no_strict_choice(
        cx: &mut TestAppContext,
    ) {
        let mut h = open(cx, snapshot("sender"));
        click(&h, cx, &format!("prepare-budget-{}", "2".repeat(64)));
        let values = h.view.read_with(cx, |view, cx| {
            let form = view.form.as_ref().unwrap();
            assert!(
                !form
                    .fields
                    .iter()
                    .any(|field| field.key == "acceptance" || field.key == "budget_id")
            );
            form.values(cx)
        });
        assert_eq!(values["budget_id"], "2".repeat(64));
        assert_eq!(values["mode"], "paid");
        assert_eq!(values["max_gas_amount"], "7");
        click(&h, cx, "save-form");
        match command(&mut h) {
            Command::Save { values, .. } => {
                assert_eq!(values["budget_id"], "2".repeat(64));
                assert!(!values.contains_key("acceptance"));
            }
            _ => panic!("expected only preparation intent"),
        }
    }
    #[gpui_kit::test]
    fn permission_loss_disables_the_frozen_financial_confirmation(cx: &mut TestAppContext) {
        let mut h = open(cx, snapshot("sender"));
        click(&h, cx, &format!("confirm-batch-{}", "2".repeat(64)));
        h.view.update(cx, |view, cx| {
            view.snapshot = Arc::new(Snapshot {
                can_manage: false,
                ..(*view.snapshot).clone()
            });
            cx.notify();
        });
        click(&h, cx, "confirm-danger");
        assert!(h.commands.try_recv().is_err());
    }
    #[test]
    fn batch_counterparties_can_differ_while_the_acting_node_stays_fixed() {
        let sender = order('d', "sender");
        let mut other_receiver = order('e', "sender");
        other_receiver.receiver_peer = "9".repeat(64);
        assert!(
            validate_transfer_pair(&[sender.clone(), other_receiver.clone()], false, 1).is_ok()
        );
        assert_eq!(
            transfer_batches(&[sender.clone(), other_receiver.clone()], false, 1).len(),
            1
        );
        other_receiver.source_peer = "8".repeat(64);
        assert!(validate_transfer_pair(&[sender, other_receiver], false, 1).is_err());

        let receiver = order('d', "receiver");
        let mut other_sender = order('e', "receiver");
        other_sender.source_peer = "9".repeat(64);
        assert!(validate_transfer_pair(&[receiver.clone(), other_sender.clone()], true, 1).is_ok());
        assert_eq!(
            transfer_batches(&[receiver.clone(), other_sender.clone()], true, 1).len(),
            1
        );
        other_sender.receiver_peer = "8".repeat(64);
        assert!(validate_transfer_pair(&[receiver, other_sender], true, 1).is_err());
    }

    #[test]
    fn cross_budget_or_third_order_never_becomes_an_automatic_pair() {
        let a = order('d', "sender");
        let mut b = order('e', "sender");
        b.payment.budget_id = Some("3".repeat(64));
        assert!(validate_transfer_pair(&[a.clone(), b.clone()], false, 1).is_err());
        assert!(transfer_batches(&[a.clone(), b], false, 1).is_empty());
        let b = order('e', "sender");
        let mut c = b.clone();
        c.order_id = "9".repeat(64);
        c.intent_id = "00000000-0000-4000-8000-000000000003".into();
        assert!(transfer_batches(&[a, b, c], false, 1).is_empty());
    }
    #[test]
    fn public_budget_and_publication_states_preserve_unknown_and_historical_strict() {
        let mut value = serde_json::to_value(budget()).unwrap();
        value["free"] = json!("99");
        assert!(parse_transfer_budgets(json!({"budgets":[value]})).is_err());
        assert!(parse_transfer_budgets(json!({"budgets":[budget(),budget()]})).is_err());
        let mut order = order('d', "sender");
        order.payment.settlement = Some(TransferSettlement {
            earned: "20".into(),
            withdrawn: "10".into(),
            refunded: "0".into(),
            publication: Some(TransferPublication {
                state: TransferPublicationState::DisclosedOutstanding,
                total_amount: "20".into(),
                economic_cutoff: 2000000200,
                availability_risk: Some("节点可用性待确认".into()),
            }),
        });
        let decoded: TransferOrder =
            serde_json::from_value(serde_json::to_value(&order).unwrap()).unwrap();
        assert_eq!(
            decoded
                .payment
                .settlement
                .as_ref()
                .unwrap()
                .publication
                .as_ref()
                .unwrap()
                .availability_risk
                .as_deref(),
            Some("节点可用性待确认")
        );
        assert_eq!(
            transfer_publication_label(&decoded),
            "恢复材料可能已公开，正在确认收入"
        );
        order.payment.state = "unknown".into();
        assert_eq!(transfer_payment_label(&order.payment.state), "付款结果未知");
        assert_eq!(
            transfer_publication_label(&order),
            "批次状态待确认，请检查原交易"
        );
        order.acceptance = "strict".into();
        order.payment.budget_id = None;
        assert!(validate_transfer_order(&order).is_ok());
        assert!(!transfer_confirmable(&order, 1));
        assert!(!transfer_acceptable(&order));
        assert_eq!(
            transfer_sum(["340282366920938463463374607431768211455", "1"]).unwrap(),
            "340282366920938463463374607431768211456"
        );
    }
}

mod explicit_delivery_restart {
    use super::*;
    use std::collections::BTreeMap;
    fn completed() -> ui::TransferOrder {
        serde_json::from_value(serde_json::json!({"order_id":"d".repeat(64),"intent_id":"00000000-0000-4000-8000-000000000001",
            "role":"sender","mode":"charity","acceptance":"recoverable","source_peer":"a".repeat(64),
            "receiver_peer":"b".repeat(64),"provider_peer":"c".repeat(64),"name":"file.bin","bytes":4,"sha256":"f".repeat(64),
            "phase":"recoverable","evidence":"charity_receipt","windows":[{"index":0,"bytes":4,"committed_bytes":4,"state":"held","decryption_proven":false}],
            "payment":{"state":"free","delivery_fee":"0","reservation_fee":"0","priority_fee":"0","total":"0","gas_cap":"0","earned":"0","withdrawn":"0","refunded":"0"}})).unwrap()
    }
    #[gpui_kit::test]
    fn completed_sender_has_explicit_restart_entry_that_only_requests_review(
        cx: &mut TestAppContext,
    ) {
        let order = completed();
        let mut state = fixture();
        state.can_manage = true;
        state.delivery.orders = vec![order.clone()];
        let mut h = open(cx, Route::Transfers, state);
        click(&h, cx, &format!("restart-{}", order.order_id));
        let (generation, command) = scoped(h.commands.try_recv().unwrap());
        assert_eq!(generation, 7);
        let Command::DeliveryRestart(review) = command else {
            panic!("restart must only request a fresh original-order review");
        };
        assert_eq!(review.intent_id, order.intent_id);
        assert_eq!(review.order_id, order.order_id);
        assert!(
            h.commands.try_recv().is_err(),
            "restart click cannot confirm or submit a prepare operation"
        );
    }
    #[gpui_kit::test]
    fn restart_form_keeps_original_context_immutable_and_requires_file_reselection(
        cx: &mut TestAppContext,
    ) {
        let h = open(cx, Route::Transfers, fixture());
        let order = completed();
        let review = serde_json::to_string(&order).unwrap();
        h.window
            .update(cx, |_, window, cx| {
                h.view.update(cx, |view, cx| {
                    view.edit(
                        views::forms::Editor::Transfer,
                        BTreeMap::from([
                            ("_restart_order_id", order.order_id.clone()),
                            ("_restart_intent_id", order.intent_id.clone()),
                            ("_restart_review", review.clone()),
                            ("budget_id", "budget-original".into()),
                            ("receiver_peer", order.receiver_peer.clone()),
                            ("max_service_amount", "0".into()),
                        ]),
                        window,
                        cx,
                    );
                    let form = view.form.as_ref().unwrap();
                    assert!(form.fields.iter().all(
                        |field| !field.key.starts_with("_restart") && field.key != "budget_id"
                    ));
                    let values = form.values(cx);
                    assert_eq!(values["_restart_order_id"], order.order_id);
                    assert_eq!(values["_restart_intent_id"], order.intent_id);
                    assert_eq!(values["_restart_review"], review);
                    assert_eq!(values["budget_id"], "budget-original");
                    assert_eq!(
                        values["file"], "",
                        "restart never reuses a prior local path implicitly"
                    );
                });
            })
            .unwrap();
    }
    #[test]
    fn unknown_and_unmanaged_snapshots_do_not_offer_restart() {
        let mut state = fixture();
        state.can_manage = true;
        let mut order = completed();
        assert!(Command::DeliveryRestart(order.clone()).available(&state));
        order.phase = "transferring".into();
        assert!(!Command::DeliveryRestart(order).available(&state));
        state.can_manage = false;
        assert!(!Command::DeliveryRestart(completed()).available(&state));
    }
}

#[gpui_kit::test]
fn critical_events_are_drained_after_snapshot_sender_closes(cx: &mut TestAppContext) {
    let mut h = open(cx, Route::Chat("chat-a".into()), fixture());
    enter_message(&h, cx, "queued input before backend shutdown");
    click(&h, cx, "send");
    let ack = next_composer_ack(&mut h);
    assert!(h.view.read_with(cx, |view, _| view.sending));
    let Harness {
        view,
        events,
        _snapshots: snapshots,
        ..
    } = h;
    drop(snapshots);
    cx.run_until_parked();
    events
        .try_send(Event::Scoped {
            generation: 7,
            profile_id: None,
            event: Box::new(Event::Notice("final backend notice".into())),
        })
        .unwrap();
    events
        .try_send(Event::Scoped {
            generation: 7,
            profile_id: None,
            event: Box::new(Event::InputStored(ack)),
        })
        .unwrap();
    drop(events);
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        assert_eq!(view.notice.as_deref(), Some("final backend notice"));
        assert!(!view.notice_is_error);
        assert!(!view.sending);
        assert_eq!(view.composer.read(cx).value().as_str(), "");
    });
}
