use crate::{
    app::Desktop,
    ui::*,
    views::{
        backend::{Command, HostAction, IdentityAction, SettingsKind},
        forms::Editor,
    },
};
use gpui_kit::{
    component::{Disableable, button::Button},
    *,
};
use std::collections::BTreeMap;

const RELAY_FEE_AUTHORIZATION_NOTE: &str =
    "中转设备的链费支出沿用本机既有授权、限额和公开规则；付款方的金额不是中转设备的支出限额。";

fn order_fee_authorization_label(order: &TransferOrder) -> String {
    if order.mode == "charity" {
        "免费传输，不使用预付预算".into()
    } else if order.role == "sender" {
        format!("原付款方累计链费上限 {}", order.payment.gas_cap)
    } else {
        "此订单视图不提供中转设备的本机链费授权".into()
    }
}

impl Desktop {
    pub(crate) fn devices(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut page = self
            .page()
            .child(self.button(
                "invite",
                "生成邀请",
                Command::Identity(IdentityAction::Invite),
                cx,
            ))
            .child(self.button(
                "join-requests",
                "读取加入请求",
                Command::Identity(IdentityAction::Requests),
                cx,
            ));
        for member in &self.snapshot.members {
            let mut row = self.row(
                member.name.clone(),
                format!(
                    "{} · {}",
                    member.kind,
                    if member.manage {
                        "管理设备"
                    } else {
                        "普通成员"
                    }
                ),
            );
            if self.snapshot.can_manage {
                row = row
                    .child(self.open_editor(
                        SharedString::from(format!("rename-{}", member.device_id)),
                        "重命名",
                        Editor::Member(member.device_id.clone()),
                        BTreeMap::from([("name", member.name.clone())]),
                        cx,
                    ))
                    .child(self.button(
                        SharedString::from(format!("grant-{}", member.device_id)),
                        if member.manage {
                            "取消管理权限"
                        } else {
                            "授予管理权限"
                        },
                        Command::Identity(IdentityAction::Grant {
                            device_id: member.device_id.clone(),
                            manage: !member.manage,
                        }),
                        cx,
                    ))
                    .child(self.ask(
                        SharedString::from(format!("revoke-{}", member.device_id)),
                        "撤销成员",
                        Command::Identity(IdentityAction::Revoke(member.device_id.clone())),
                        cx,
                    ));
            }
            page = page.child(row);
        }
        for join in &self.snapshot.joins {
            let mut row = self.row(
                join.name.clone(),
                join.code
                    .as_ref()
                    .map(|code| format!("核对短码：{code}"))
                    .unwrap_or_else(|| "先发送核对请求，再在两台设备比较短码。".into()),
            );
            if join.code.is_none() {
                row = row.child(self.button(
                    SharedString::from(format!("join-confirm-{}", join.join_id)),
                    "开始短码核对",
                    Command::Identity(IdentityAction::ConfirmJoin(join.join_id.clone())),
                    cx,
                ));
            } else {
                row = row
                    .child(self.ask(
                        SharedString::from(format!("join-grant-{}", join.join_id)),
                        "短码一致，允许加入",
                        Command::Identity(IdentityAction::GrantJoin {
                            join_id: join.join_id.clone(),
                            manage: false,
                        }),
                        cx,
                    ))
                    .child(self.ask(
                        SharedString::from(format!("join-manager-{}", join.join_id)),
                        "短码一致，作为管理设备加入",
                        Command::Identity(IdentityAction::GrantJoin {
                            join_id: join.join_id.clone(),
                            manage: true,
                        }),
                        cx,
                    ));
            }
            page = page.child(row.child(self.button(
                SharedString::from(format!("join-reject-{}", join.join_id)),
                "拒绝加入",
                Command::Identity(IdentityAction::RejectJoin(join.join_id.clone())),
                cx,
            )));
        }
        for node in &self.snapshot.nodes {
            page = page.child(
                self.row(
                    node.name.clone(),
                    if node.revoked {
                        "已撤销"
                    } else if self.snapshot.cached || !self.snapshot.online {
                        "当前状态未验证，显示上次记录"
                    } else if node.online {
                        "在线"
                    } else {
                        "离线，状态待确认"
                    },
                )
                .child(self.button(
                    SharedString::from(format!("settings-{}", node.device_id)),
                    "读取配置",
                    Command::NodeSettings(node.device_id.clone()),
                    cx,
                )),
            );
        }
        page = page
            .child(self.button(
                "provider",
                "配置 Codex",
                Command::EditSettings(SettingsKind::Provider(vibeface_client::Provider::Codex)),
                cx,
            ))
            .child(self.button(
                "node-settings-editor",
                "编辑 Node 配置",
                Command::EditSettings(SettingsKind::Node),
                cx,
            ))
            .child(self.button(
                "triage",
                "配置 triage 与 webhook",
                Command::EditSettings(SettingsKind::Triage),
                cx,
            ))
            .child(self.button(
                "relay",
                "配置 relay",
                Command::EditSettings(SettingsKind::Relay),
                cx,
            ));
        for (provider, label) in [
            (vibeface_client::Provider::Codex, "Codex"),
            (vibeface_client::Provider::Cursor, "Cursor"),
            (vibeface_client::Provider::Claude, "Claude"),
            (vibeface_client::Provider::Pi, "Pi"),
            (vibeface_client::Provider::Opencode, "OpenCode"),
        ] {
            page = page.child(
                self.row(label, "读取就绪状态与可用模型后再配置。")
                    .child(self.button(
                        SharedString::from(format!("provider-edit-{label}")),
                        "编辑配置",
                        Command::EditSettings(SettingsKind::Provider(provider.clone())),
                        cx,
                    ))
                    .child(self.button(
                        SharedString::from(format!("provider-models-{label}")),
                        "读取模型",
                        Command::ProviderRead {
                            provider: provider.clone(),
                            probe: false,
                        },
                        cx,
                    ))
                    .child(self.button(
                        SharedString::from(format!("provider-probe-{label}")),
                        "探测可用性",
                        Command::ProviderRead {
                            provider,
                            probe: true,
                        },
                        cx,
                    )),
            );
        }
        for token in &self.snapshot.decision_tokens {
            page = page.child(
                self.row(
                    token.label.clone(),
                    format!("创建时间 {}", token.created_ms),
                )
                .child(self.ask(
                    SharedString::from(format!("revoke-token-{}", token.token_id)),
                    "撤销授权码",
                    Command::DecisionTokenRevoke(token.token_id.clone()),
                    cx,
                )),
            );
        }
        page = page
            .child(self.open_editor(
                "create-decision-token",
                "创建并复制新授权码",
                Editor::DecisionToken,
                BTreeMap::new(),
                cx,
            ))
            .child(self.button(
                "refresh-decision-tokens",
                "读取现有授权码",
                Command::ReadTriage,
                cx,
            ));
        page = page.child(self.button("test-webhook", "测试 Webhook", Command::TestWebhook, cx));
        page.into_any_element()
    }
    pub(crate) fn settings(&self, cx: &mut Context<Self>) -> AnyElement {
        self.page()
            .child(self.profile_picker(cx))
            .child(self.row("外观", "与现有 Vibeface 的中性色与紫色焦点保持一致。"))
            .child(
                Button::new("theme")
                    .label(if self.dark {
                        "切换浅色"
                    } else {
                        "切换深色"
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dark = !this.dark;
                        crate::theme::apply(this.dark, window, cx);
                        if let Some(host) = &this.host {
                            let mut preferences = host.preferences.clone();
                            preferences.theme = if this.dark {
                                crate::host::Theme::Dark
                            } else {
                                crate::host::Theme::Light
                            };
                            this.send(Command::Host(HostAction::Preferences(preferences)), cx);
                        }
                        cx.notify();
                    })),
            )
            .child(
                self.button(
                    "system-theme",
                    "跟随系统外观",
                    Command::Host(HostAction::Preferences(crate::host::Preferences {
                        theme: crate::host::Theme::System,
                        ..self
                            .host
                            .as_ref()
                            .map(|h| h.preferences.clone())
                            .unwrap_or_default()
                    })),
                    cx,
                ),
            )
            .child(
                self.button(
                    "notifications",
                    if self
                        .host
                        .as_ref()
                        .map(|host| host.preferences.notifications)
                        .unwrap_or(true)
                    {
                        "关闭系统通知"
                    } else {
                        "开启系统通知"
                    },
                    Command::Host(HostAction::Preferences(crate::host::Preferences {
                        notifications: !self
                            .host
                            .as_ref()
                            .map(|h| h.preferences.notifications)
                            .unwrap_or(true),
                        ..self
                            .host
                            .as_ref()
                            .map(|h| h.preferences.clone())
                            .unwrap_or_default()
                    })),
                    cx,
                ),
            )
            .child(
                self.button(
                    "business-on-connect",
                    if self
                        .host
                        .as_ref()
                        .map(|host| host.preferences.open_business_on_connect)
                        .unwrap_or(true)
                    {
                        "连接后保留当前页面"
                    } else {
                        "连接后打开Bot列表"
                    },
                    Command::Host(HostAction::Preferences(crate::host::Preferences {
                        open_business_on_connect: !self
                            .host
                            .as_ref()
                            .map(|host| host.preferences.open_business_on_connect)
                            .unwrap_or(true),
                        ..self
                            .host
                            .as_ref()
                            .map(|host| host.preferences.clone())
                            .unwrap_or_default()
                    })),
                    cx,
                )
                .disabled(self.host.is_none()),
            )
            .child(self.nav("logs", "日志与诊断", Route::Logs, cx))
            .child(self.nav("transfers", "实验与文件传输", Route::Transfers, cx))
            .child(self.nav("join", "加入与恢复", Route::Join, cx))
            .child(self.ask(
                "confirm-writable",
                "核对并恢复写入",
                Command::ConfirmWritable,
                cx,
            ))
            .child(self.button("disconnect", "断开当前连接", Command::Disconnect, cx))
            .into_any_element()
    }
    pub(crate) fn node(&self, cx: &mut Context<Self>) -> AnyElement {
        let runtime_ready = self
            .host
            .as_ref()
            .is_some_and(|host| matches!(host.runtime, crate::host::RuntimeState::Ready));
        let mut page = self
            .page()
            .child(self.button(
                "owner-node-settings",
                "管理所选本机Node配置",
                Command::EditOwnerSettings(SettingsKind::Node),
                cx,
            ))
            .child(self.button(
                "owner-node-provider",
                "管理所选本机Codex配置",
                Command::EditOwnerSettings(SettingsKind::Provider(
                    vibeface_client::Provider::Codex,
                )),
                cx,
            ))
            .child("选中的 Node、应用启动的 Node 和后台服务分别管理。扫描和选择不会启动 Node。")
            .child(self.button(
                "node-refresh",
                "重新扫描",
                Command::Host(HostAction::Refresh),
                cx,
            ))
            .child(self.button(
                "controller-mode",
                "使用纯控制端",
                Command::Host(HostAction::Controller),
                cx,
            ))
            .child(
                self.button(
                    "node-start",
                    "启动本机 Node",
                    Command::Host(HostAction::Start),
                    cx,
                )
                .disabled(
                    !runtime_ready || self.host.as_ref().is_some_and(|host| host.owned.is_some()),
                ),
            )
            .child(
                self.button(
                    "node-stop",
                    "停止应用启动的 Node",
                    Command::Host(HostAction::Stop),
                    cx,
                )
                .disabled(self.host.as_ref().is_none_or(|host| host.owned.is_none())),
            )
            .child(self.open_editor(
                "connect-local",
                "连接已选择的本机 Node",
                Editor::ConnectLocal,
                BTreeMap::new(),
                cx,
            ))
            .child(self.button(
                "clear-node",
                "忘记选择，保留数据",
                Command::Host(HostAction::ClearSelection),
                cx,
            ));
        page = page
            .child(
                Button::new("remember-node-directory")
                    .label("选择已有Node数据目录")
                    .on_click(cx.listener(|this, _, _, cx| this.choose_node_directory(cx))),
            )
            .child(
                self.button(
                    "read-background-state",
                    "读取后台服务与运行时状态",
                    Command::Host(HostAction::Background(
                        crate::host::BackgroundAction::Status,
                    )),
                    cx,
                )
                .disabled(!runtime_ready),
            );
        if let Some(host) = &self.host {
            if let Some(exit) = host.last_owned_exit {
                page = page.child(self.row("最近自有Node退出码", exit.to_string()));
            }
            if let Some(owned) = &host.owned {
                page = page.child(self.row(
                    "应用自有进程",
                    format!(
                            "{} · PID {}。仅此进程由App退出时关闭。",
                            owned.identity.device_id,
                            owned
                                .process_id
                                .map(|pid| pid.to_string())
                                .unwrap_or_else(|| "退出中".into())
                        ),
                ));
            } else {
                page = page.child("App当前没有自有Node进程。附着的后台服务不会在退出时停止。");
            }
            if !runtime_ready {
                page=page.child("当前App未提供完整Core/CLI运行时。远程控制端仍可使用；本机启动与后台管理尚不可用。");
            }
            for warning in &host.discovery.warnings {
                page = page.child(format!("扫描提示：{warning}"));
            }
            if let Some(service) = &host.background {
                let update = &service.runtime_update;
                page = page
                    .child(self.row(
                        "后台服务状态",
                        format!(
                                "{} · 服务进程 {} · Core {:?}",
                                match service.state {
                                    crate::host::ServiceState::NotInstalled => "尚未安装",
                                    crate::host::ServiceState::Installed => "已安装",
                                    crate::host::ServiceState::InstalledStopped =>
                                        "已安装，服务已停止",
                                    crate::host::ServiceState::Partial => "安装尚不完整",
                                    crate::host::ServiceState::Conflict => "安装或运行身份冲突",
                                    crate::host::ServiceState::Unavailable => "状态不可确认",
                                },
                                service
                                    .service_running
                                    .map(|running| if running { "运行中" } else { "已停止" })
                                    .unwrap_or("尚未确认"),
                                service.core_status
                            ),
                    ))
                    .child(self.row("App随附Core版本", update.bundled_version.clone()))
                    .child(
                        self.row(
                            "已安装后台版本",
                            update
                                .installed_version
                                .clone()
                                .unwrap_or_else(|| "未安装或尚未确认".into()),
                        ),
                    )
                    .child(
                        self.row(
                            "实际运行版本",
                            update
                                .running_version
                                .clone()
                                .unwrap_or_else(|| "尚未确认运行实例".into()),
                        ),
                    )
                    .child(self.row(
                        "运行时更新判断",
                        format!(
                            "{}{}",
                            match update.availability {
                                crate::host::UpdateAvailability::NotInstalled =>
                                    "后台运行时尚未安装",
                                crate::host::UpdateAvailability::Current => "随附与已安装版本一致",
                                crate::host::UpdateAvailability::NewerAvailable =>
                                    "随附版本可用于更新",
                                crate::host::UpdateAvailability::ImmutableConflict =>
                                    "同版本内容摘要冲突，拒绝覆盖",
                                crate::host::UpdateAvailability::DowngradeRefused =>
                                    "已安装版本较新，拒绝降级",
                            },
                            if update.recovery_required {
                                " · 存在待恢复更新，请保留原数据与安装目录"
                            } else {
                                ""
                            }
                        ),
                    ));
                if let Some(root) = &service.managed_root {
                    page = page.child(self.row("后台受管目录", root.display().to_string()));
                }
            } else {
                page = page.child("后台服务状态尚未读取，不把未知状态当作已安装或已停止。");
            }
            for node in &host.discovery.nodes {
                page = page.child(
                    self.row(
                        node.data_dir.display().to_string(),
                        format!(
                            "{:?} · {} · Core {}",
                            node.status,
                            node.device_id,
                            node.core_version.as_deref().unwrap_or("版本未确认")
                        ),
                    )
                    .child(self.button(
                        SharedString::from(format!("select-{}", node.device_id)),
                        "选择此 Node",
                        Command::Host(HostAction::Select(node.identity())),
                        cx,
                    ))
                    .child(self.button(
                        SharedString::from(format!("start-candidate-{}", node.device_id)),
                        "启动此 Node",
                        Command::Host(HostAction::StartCandidate(node.clone())),
                        cx,
                    )),
                );
            }
        }
        for (provider, label) in [
            (vibeface_client::Provider::Codex, "Codex"),
            (vibeface_client::Provider::Cursor, "Cursor"),
            (vibeface_client::Provider::Claude, "Claude"),
            (vibeface_client::Provider::Pi, "Pi"),
            (vibeface_client::Provider::Opencode, "OpenCode"),
        ] {
            page = page.child(self.button(
                SharedString::from(format!("owner-provider-{label}")),
                format!("管理本机{label}配置"),
                Command::EditOwnerSettings(SettingsKind::Provider(provider)),
                cx,
            ));
        }
        page = page
            .child(self.button(
                "owner-triage",
                "管理本机triage与Webhook",
                Command::EditOwnerSettings(SettingsKind::Triage),
                cx,
            ))
            .child(self.button(
                "owner-relay",
                "管理本机relay",
                Command::EditOwnerSettings(SettingsKind::Relay),
                cx,
            ));
        for (label, action) in [
            ("安装后台服务", crate::host::BackgroundAction::Install),
            ("启动后台服务", crate::host::BackgroundAction::Start),
            ("停止后台服务", crate::host::BackgroundAction::Stop),
            ("更新后台服务", crate::host::BackgroundAction::Update),
            ("移除后台服务", crate::host::BackgroundAction::Uninstall),
        ] {
            let enabled = runtime_ready
                && self
                    .host
                    .as_ref()
                    .and_then(|host| host.background.as_ref())
                    .is_some_and(|service| match action {
                        crate::host::BackgroundAction::Install => {
                            service.state == crate::host::ServiceState::NotInstalled
                        }
                        crate::host::BackgroundAction::Start => {
                            service.state == crate::host::ServiceState::InstalledStopped
                        }
                        crate::host::BackgroundAction::Stop
                        | crate::host::BackgroundAction::Uninstall => matches!(
                            service.state,
                            crate::host::ServiceState::Installed
                                | crate::host::ServiceState::InstalledStopped
                                | crate::host::ServiceState::Partial
                        ),
                        crate::host::BackgroundAction::Update => {
                            service.runtime_update.recovery_required
                                || service.runtime_update.availability
                                    == crate::host::UpdateAvailability::NewerAvailable
                        }
                        crate::host::BackgroundAction::Status => true,
                    });
            page = page.child(
                self.button(
                    label,
                    label,
                    Command::Host(HostAction::Background(action)),
                    cx,
                )
                .disabled(!enabled),
            );
        }
        page.into_any_element()
    }
    pub(crate) fn transfers(&self, cx: &mut Context<Self>) -> AnyElement {
        let d = &self.snapshot.delivery;
        let mut page = self
            .page()
            .child(format!(
                "钱包状态 {} · {}",
                d.wallet.state,
                d.wallet.reason.clone().unwrap_or_default()
            ))
            .child("付费结果未知时先查询原订单，不能另建订单付款。")
            .child(self.button(
                "transfer-toggle",
                if d.enabled {
                    "关闭文件传输"
                } else {
                    "启用免费文件传输"
                },
                Command::DeliveryConfigure {
                    enabled: !d.enabled,
                    charity: true,
                    paid: d.paid_enabled,
                },
                cx,
            ))
            .child(self.button(
                "paid-toggle",
                if d.paid_enabled {
                    "关闭付费实验"
                } else {
                    "启用付费实验"
                },
                Command::DeliveryConfigure {
                    enabled: d.enabled,
                    charity: d.charity_enabled,
                    paid: !d.paid_enabled,
                },
                cx,
            ))
            .child(self.open_editor(
                "transfer-prepare",
                "准备文件传输",
                Editor::Transfer,
                BTreeMap::from([
                    ("mode", "charity".into()),
                    ("acceptance", "recoverable".into()),
                    ("max_service_amount", "0".into()),
                    ("max_gas_amount", "0".into()),
                ]),
                cx,
            ))
            .child(self.button("transfer-refresh", "刷新传输记录", Command::Transfers, cx));
        let now = vibeface_protocol::now_ms() / 1000;
        if let Some(error) = &d.budget_error {
            page = page.child(self.row("预算状态待核对", error.clone()));
        }
        page=page.child("受保护付费使用已有预付预算，仅可恢复验收；金额均为代币或原生币的最小单位整数，不填写小数。预算开户、充值和钱包解锁请在本机使用现有CLI维护。历史Strict订单仅保留查看和文件读取。");
        for budget in &d.budgets {
            let mut row = self.row(
                format!("预算 {} · {}", budget.budget_id, budget.state),
                format!(
                    "中转 {}
固定付款人 {} · 收款人 {} · 资产 {}
账本存入 {} · 可用 {} · 锁定 {}
预算账本累计赚取 {} · 提取 {} · 退款 {}（不作到账确认）
本机记录的付款方累计链费上限 {} · 就绪 {} 片",
                    budget.provider_peer,
                    budget.payer,
                    budget.payee,
                    budget.asset,
                    budget.deposit,
                    budget.free,
                    budget.locked,
                    budget.earned,
                    budget.withdrawn,
                    budget.refunded,
                    budget.gas_cap,
                    budget.ready_windows.len()
                ),
            );
            if budget.provider_peer == d.node_peer {
                row = row.child(RELAY_FEE_AUTHORIZATION_NOTE);
            }
            let offer = d
                .providers
                .iter()
                .find(|provider| provider.device_id == budget.provider_peer && provider.paid)
                .and_then(|provider| provider.offer.as_ref())
                .and_then(|offer| offer.settlement.as_ref());
            if budget.state == "open"
                && d.wallet
                    .address
                    .as_deref()
                    .is_some_and(|payer| payer.eq_ignore_ascii_case(&budget.payer))
                && offer.is_some_and(|offer| offer.asset.eq_ignore_ascii_case(&budget.asset))
            {
                row = row.child(
                    self.open_editor(
                        SharedString::from(format!("prepare-budget-{}", budget.budget_id)),
                        "使用此预算准备付费传输",
                        Editor::Transfer,
                        BTreeMap::from([
                            ("mode", "paid".into()),
                            ("budget_id", budget.budget_id.clone()),
                            ("provider_peer", budget.provider_peer.clone()),
                            ("max_service_amount", budget.free.clone()),
                            ("max_gas_amount", budget.gas_cap.clone()),
                        ]),
                        cx,
                    )
                    .disabled(
                        !self.snapshot.online
                            || !self.snapshot.can_manage
                            || !d.enabled
                            || !d.paid_enabled,
                    ),
                );
            }
            if budget.provider_peer == d.node_peer
                && budget.state != "closed"
                && let Ok(review) = transfer_flush_review(budget, &d.orders)
            {
                row = row.child(
                    self.ask(
                        SharedString::from(format!("flush-budget-{}", budget.budget_id)),
                        "核对并请求结算此预算就绪片段",
                        Command::DeliveryFlushBudget(Box::new(review)),
                        cx,
                    )
                    .disabled(!self.snapshot.online || !self.snapshot.can_manage),
                );
            }
            page = page.child(row);
        }
        for (accept, label) in [(false, "确认两笔原订单"), (true, "接收两笔原订单")] {
            for orders in transfer_batches(&d.orders, accept, now) {
                let total = transfer_sum(orders.iter().map(|order| order.payment.total.as_str()))
                    .unwrap_or_else(|_| "待核对".into());
                let budget = orders[0].payment.budget_id.clone().unwrap_or_default();
                let mut row =
                    self.row(label, format!("固定预算 {budget} · 两单服务费合计 {total}"));
                for order in &orders {
                    row = row.child(self.transfer_order_review(order));
                }
                let command = if accept {
                    Command::DeliveryAcceptBatch(Box::new(orders))
                } else {
                    Command::DeliveryConfirmBatch(Box::new(orders))
                };
                row = row.child(
                    self.ask(
                        SharedString::from(format!(
                            "{}-{budget}",
                            if accept {
                                "accept-batch"
                            } else {
                                "confirm-batch"
                            }
                        )),
                        label,
                        command,
                        cx,
                    )
                    .disabled(
                        !self.snapshot.online || !self.snapshot.can_manage || !d.paid_enabled,
                    ),
                );
                page = page.child(row);
            }
        }
        for provider in &d.providers {
            page = page.child(self.row(
                format!("传输服务设备 {}", provider.device_id),
                format!(
                        "{} · 每窗口价格 {}",
                        if provider.paid {
                            "支持付费"
                        } else {
                            "免费服务"
                        },
                        provider
                            .offer
                            .as_ref()
                            .map(|offer| offer.price.as_str())
                            .unwrap_or("未报价")
                    ),
            ));
        }
        for provider in &d.providers {
            if let Some(settlement) = provider
                .offer
                .as_ref()
                .and_then(|offer| offer.settlement.as_ref())
            {
                page = page.child(self.row(
                    format!("中转设备 {} 的结算配置", provider.device_id),
                    format!(
                        "链 {} · 托管合约 {} · 资产 {}",
                        settlement.chain_id, settlement.escrow, settlement.asset
                    ),
                ));
            }
        }
        for order in &d.orders {
            let mut row = self
                .row(
                    order.name.clone().unwrap_or_else(|| "加密文件".into()),
                    format!("{} · {} · {} 字节", order.mode, order.phase, order.bytes),
                )
                .child(format!(
                    "接收设备 {} · 传输设备 {}",
                    order.receiver_peer, order.provider_peer
                ))
                .child(format!(
                    "服务费 {} · {} · 付款状态 {}",
                    order.payment.total,
                    order_fee_authorization_label(order),
                    if transfer_settlement_uncertain(order) {
                        format!(
                            "上次记录 {}（当前待确认）",
                            transfer_payment_label(&order.payment.state)
                        )
                    } else {
                        transfer_payment_label(&order.payment.state).to_owned()
                    }
                ))
                .child(format!(
                    "传输费 {} · 暂存预留费 {} · 时效优先费 {}",
                    order.payment.delivery_fee,
                    order.payment.reservation_fee,
                    order.payment.priority_fee
                ))
                .child(format!(
                    "付款钱包 {} · 固定收款钱包 {} · 链 {} · 资产 {}",
                    order.payment.payer.as_deref().unwrap_or("免费"),
                    order.payment.provider_wallet.as_deref().unwrap_or("免费"),
                    order.payment.chain_id.as_deref().unwrap_or("无"),
                    order.payment.asset.as_deref().unwrap_or("无")
                ));
            if let Some(deadlines) = &order.deadlines {
                row=row.child(format!("报价截止 {} · 持有截止 {} · 发布密钥截止 {} · 解密截止 {} · 保留至 {} · 时效截止 {}",deadlines.quote_expires_at,deadlines.hold_deadline,deadlines.key_deadline,deadlines.open_deadline,deadlines.retain_until,deadlines.priority_deadline));
            }
            for window in &order.windows {
                row = row.child(format!(
                    "窗口 {} · {} · 密文已落盘 {} / {} 字节 · 解密证明 {}",
                    window.index + 1,
                    window.state,
                    window.committed_bytes,
                    window.bytes + 68,
                    window.decryption_proven
                ));
            }
            let restart = Command::DeliveryRestart(order.clone());
            if restart.available(&self.snapshot) {
                row = row.child(self.button(
                    SharedString::from(format!("restart-{}", order.order_id)),
                    "作为新订单再次发送",
                    restart,
                    cx,
                ));
            }
            if transfer_confirmable(order, now) {
                row = row.child(self.ask(
                    SharedString::from(format!("confirm-{}", order.order_id)),
                    if order.mode == "paid" {
                        "确认条款并付款"
                    } else {
                        "确认免费发送"
                    },
                    Command::DeliveryConfirm(order.clone()),
                    cx,
                ));
            }
            if transfer_acceptable(order) {
                row = row.child(self.ask(
                    SharedString::from(format!("accept-{}", order.order_id)),
                    "接收文件",
                    Command::DeliveryAccept(order.clone()),
                    cx,
                ));
            }
            if order.mode == "paid" {
                let uncertain = transfer_settlement_uncertain(order);
                row = row
                    .child(format!(
                        "{}：{} / {}",
                        if uncertain {
                            "本单上次记录（当前待确认）"
                        } else {
                            "本单已赚取 / 已退款"
                        },
                        order.payment.earned,
                        order.payment.refunded
                    ))
                    .child(format!(
                        "固定预算 {} · {}",
                        order
                            .payment
                            .budget_id
                            .as_deref()
                            .unwrap_or("历史订单未提供"),
                        transfer_publication_label(order)
                    ));
                if let Some(settlement) = &order.payment.settlement {
                    row = row.child(format!(
                        "{}：{} / {} / {}",
                        if uncertain {
                            "预算上次累计记录（到账待确认）"
                        } else {
                            "预算累计赚取 / 提取 / 退款"
                        },
                        settlement.earned,
                        settlement.withdrawn,
                        settlement.refunded
                    ));
                    if let Some(publication) = &settlement.publication {
                        row = row.child(format!(
                            "批次金额 {} · 经济截止（Unix秒） {}",
                            publication.total_amount, publication.economic_cutoff
                        ));
                        if let Some(risk) = &publication.availability_risk {
                            row = row.child(format!("可用性风险：{risk}"));
                        }
                    }
                }
                if order.acceptance == "strict" {
                    row = row.child("历史Strict订单：不提供新的准备、付款或接收操作。");
                }
            }
            let save_order = order.clone();
            row = row
                .child(
                    self.button(
                        SharedString::from(format!("check-{}", order.order_id)),
                        "查询原订单进度",
                        Command::DeliveryReconcile(order.order_id.clone()),
                        cx,
                    )
                    .disabled(order.mode == "paid" && order.acceptance == "strict"),
                )
                .child(
                    Button::new(SharedString::from(format!("save-{}", order.order_id)))
                        .label("保存文件")
                        .disabled(!can_read_transfer(order))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.save_transfer(save_order.clone(), cx)
                        })),
                );
            if let Some(error) = &order.error {
                row = row.child(error.clone());
            }
            page = page.child(row);
        }
        page.into_any_element()
    }
    pub(crate) fn choose_node_directory(&mut self, cx: &mut Context<Self>) {
        let generation = self.snapshot.generation;
        let network = self.snapshot.network.clone();
        let picker = crate::host::platform::choose_directory(cx);
        cx.spawn(async move |this, cx| match picker.await {
            Ok(Some(path)) => {
                let _ = this.update(cx, |this, cx| {
                    if this.same_view_scope(generation, &network) {
                        this.send(Command::Host(HostAction::Remember(path)), cx);
                    }
                });
            }
            Ok(None) => {}
            Err(error) => {
                let _ = this.update(cx, |this, cx| {
                    if this.same_view_scope(generation, &network) {
                        this.notice = Some(format!("目录选择失败：{error}"));
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    pub(crate) fn unknown_settings_review(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = &self.snapshot.settings_review else {
            return div().into_any_element();
        };
        let review = &view.review;
        let mut page = div().flex().flex_col().gap_3().child(self.row(
            "核对原设置操作",
            format!(
                "操作 {} · Node {}。服务器结果仍未知；关闭不会记为成功。",
                review.operation_id, review.target_node
            ),
        ));
        for (name, value) in &view.original_rows {
            page = page.child(self.row(format!("原意图 · {name}"), value.clone()));
        }
        if let Some(error) = &view.read_error {
            page = page.child(format!(
                "当前配置尚未读取：{error}。只有你已在原设备手动核对，才能说明依据后关闭。"
            ));
        } else {
            for field in view.current.iter().filter(|field| !field.secret) {
                page =
                    page.child(self.row(format!("当前状态 · {}", field.name), field.value.clone()));
            }
        }
        if review.has_secret_changes {
            page = page
                .child("本次包含密钥修改。读回不会提供这些值，不能把其他字段一致当作密钥已应用。");
        }
        page = page
            .child(format!(
                "关闭还会取消 {} 项尚未发送的依赖或同序列后续操作。",
                review.dependent_ids.len()
            ))
            .child(self.button(
                "reconcile-reviewed-settings",
                "查询原回执或可验证状态",
                Command::Reconcile(review.operation_id.clone()),
                cx,
            ))
            .child(self.open_editor(
                "close-reviewed-settings",
                "说明核对依据，关闭未知提醒",
                Editor::CloseSettings(review.clone()),
                BTreeMap::new(),
                cx,
            ));
        page.into_any_element()
    }

    pub(crate) fn save_transfer(&mut self, order: TransferOrder, cx: &mut Context<Self>) {
        let generation = self.snapshot.generation;
        let network = self.snapshot.network.clone();
        let directory = std::env::temp_dir();
        let picker = crate::host::platform::choose_save_path(
            cx,
            &directory,
            order.name.as_deref().unwrap_or("vibeface-file"),
        );
        cx.spawn(async move |this, cx| match picker.await {
            Ok(Some(path)) => {
                let _ = this.update(cx, |this, cx| {
                    if this.same_view_scope(generation, &network) {
                        this.send(Command::DeliverySave { order, path }, cx);
                    }
                });
            }
            Ok(None) => {}
            Err(error) => {
                let _ = this.update(cx, |this, cx| {
                    this.notice = Some(format!("保存对话框失败：{error}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }
    pub(crate) fn save_diagnostics(&mut self, cx: &mut Context<Self>) {
        let generation = self.snapshot.generation;
        let network = self.snapshot.network.clone();
        let picker = crate::host::platform::choose_save_path(
            cx,
            &std::env::temp_dir(),
            "vibeface-diagnostics.json",
        );
        cx.spawn(async move |this, cx| {
            if let Ok(Some(path)) = picker.await {
                let _ = this.update(cx, |this, cx| {
                    if this.same_view_scope(generation, &network) {
                        this.send(Command::DiagnosticsSave(path), cx);
                    }
                });
            }
        })
        .detach();
    }
}

impl Desktop {
    fn transfer_order_review(&self, order: &TransferOrder) -> AnyElement {
        let mut review = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.row(
                order.name.clone().unwrap_or_else(|| "加密文件".into()),
                format!(
                    "原订单 {}\n原意图 {}\n{} 字节 · 摘要 {}",
                    order.order_id,
                    order.intent_id,
                    order.bytes,
                    order.sha256.as_deref().unwrap_or("未提供")
                ),
            ))
            .child(format!(
                "来源 {} · 接收 {} · 中转 {}",
                order.source_peer, order.receiver_peer, order.provider_peer
            ))
            .child(format!(
                "固定预算 {}\n付款人 {} · 收款人 {}\n链 {} · 资产 {}",
                order.payment.budget_id.as_deref().unwrap_or("免费，无预算"),
                order.payment.payer.as_deref().unwrap_or("无"),
                order.payment.provider_wallet.as_deref().unwrap_or("无"),
                order.payment.chain_id.as_deref().unwrap_or("无"),
                order.payment.asset.as_deref().unwrap_or("无")
            ))
            .child(format!(
                "原服务费 {} · {}个片段",
                order.payment.total,
                order.windows.len()
            ))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "delivery-order-fee-{}",
                        order.order_id
                    )))
                    .role(Role::Note)
                    .aria_label(order_fee_authorization_label(order))
                    .child(order_fee_authorization_label(order)),
            )
            .child(transfer_publication_label(order));
        if let Some(deadline) = &order.deadlines {
            review = review.child(format!(
                "原截止时间（Unix秒）：报价 {} · 持有 {} · 密钥 {} · 打开 {} · 保留 {} · 时效 {}",
                deadline.quote_expires_at,
                deadline.hold_deadline,
                deadline.key_deadline,
                deadline.open_deadline,
                deadline.retain_until,
                deadline.priority_deadline
            ));
        }
        if let Some(risk) = order
            .payment
            .settlement
            .as_ref()
            .and_then(|s| s.publication.as_ref())
            .and_then(|p| p.availability_risk.as_ref())
        {
            review = review.child(format!("可用性风险：{risk}"));
        }
        review.into_any_element()
    }
    pub(crate) fn delivery_confirmation(&self, command: &Command) -> Option<AnyElement> {
        let mut review = div()
            .id("delivery-financial-review")
            .role(Role::Group)
            .flex()
            .flex_col()
            .gap_3();
        match command {
            Command::DeliveryConfirm(order) | Command::DeliveryAccept(order) => {
                review = review
                    .aria_label(format!(
                        "原订单 {} · 预算 {} · 原服务费 {}",
                        order.order_id,
                        order.payment.budget_id.as_deref().unwrap_or("免费"),
                        order.payment.total
                    ))
                    .child("核对这笔原订单的固定条款")
                    .child(self.transfer_order_review(order));
            }
            Command::DeliveryConfirmBatch(orders) | Command::DeliveryAcceptBatch(orders) => {
                let total = transfer_sum(orders.iter().map(|order| order.payment.total.as_str()))
                    .unwrap_or_else(|_| "金额待核对，不能提交".into());
                review = review
                    .aria_label(format!(
                        "两笔原订单 {} 和 {} · 预算 {} · 服务费合计 {total}",
                        orders[0].order_id,
                        orders[1].order_id,
                        orders[0].payment.budget_id.as_deref().unwrap_or("未提供")
                    ))
                    .child(format!(
                        "核对以下两笔原订单，服务费合计 {total}；不创建第二笔订单。"
                    ));
                for order in orders.iter() {
                    review = review.child(self.transfer_order_review(order));
                }
                review = review
                    .child("两单使用同一固定预算；付款方累计链费授权不按两单相加，中转设备沿用其本机已有授权和限额。");
            }
            Command::DeliveryFlushBudget(flush) => {
                let budget = &flush.budget;
                review = review
                    .aria_label(format!(
                        "原预算 {} · 原就绪片段 {}",
                        budget.budget_id,
                        budget
                            .ready_windows
                            .iter()
                            .map(|window| format!("{}:{}", window.order_id, window.index))
                            .collect::<Vec<_>>()
                            .join("、")
                    ))
                    .child(format!(
                        "提交前核对的预算与片段（执行前可能变化）：{}",
                        budget.budget_id
                    ))
                    .child(format!(
                        "中转 {} · 付款人 {} · 收款人 {} · 资产 {}",
                        budget.provider_peer, budget.payer, budget.payee, budget.asset,
                    ))
                    .child(
                        div()
                            .id("delivery-payer-chain-fee-cap")
                            .role(Role::Note)
                            .aria_label(format!("本机记录的付款方累计链费上限 {}", budget.gas_cap))
                            .child(format!("本机记录的付款方累计链费上限 {}", budget.gas_cap)),
                    )
                    .child(
                        div()
                            .id("delivery-provider-spending-note")
                            .role(Role::Note)
                            .aria_label(RELAY_FEE_AUTHORIZATION_NOTE)
                            .child(RELAY_FEE_AUTHORIZATION_NOTE),
                    );
                for window in &budget.ready_windows {
                    review = review.child(format!(
                        "原订单 {} · 原片段 {}",
                        window.order_id, window.index
                    ));
                }
                for order in &flush.orders {
                    review = review.child(self.transfer_order_review(order));
                }
                review=review.child("恢复材料可能公开。设备会在执行时选择此预算下可处理的就绪片段，列表可能在执行前变化。经济截止和可用性风险需检查原订单及后续状态；请求已接收不代表付款或收入完成。");
            }
            _ => return None,
        }
        Some(review.child("确认只提交这次意图。设备接收不代表付款、收入或到账已完成；结果未知时先检查原订单、原预算和原批次，不自动再次提交。预算建立、充值和钱包解锁仍通过发送设备现有CLI本机维护。").into_any_element())
    }
}
