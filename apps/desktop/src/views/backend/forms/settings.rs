use super::*;

impl Backend {
    pub(super) async fn save_settings(
        &mut self,
        editor: Editor,
        values: BTreeMap<String, String>,
    ) -> Result<()> {
        #[cfg(test)]
        Self::test_phase("save-settings-enter");
        let get = |key: &str| values.get(key).cloned().unwrap_or_default();
        match editor {
            Editor::CloseSettings(review) => {
                ensure!(!get("note").trim().is_empty(), "请说明你如何核对原设置");
                let closed = self
                    .client()?
                    .close_unknown_settings(review, get("note"))
                    .await?;
                self.state.settings_review = None;
                self.state.pending = self
                    .client()?
                    .pending()
                    .await?
                    .into_iter()
                    .map(|operation| Pending {
                        id: operation.id,
                        state: operation_label(operation.state).into(),
                        target_node: operation.target_node,
                    })
                    .collect();
                self.publish();
                let _=self.emit(Event::Notice(format!("已由你关闭{}项设置或依赖提醒；原服务器结果仍未知，未记为成功，原请求不会重发。",closed.len()))).await;
            }
            Editor::Logs => {
                let value = self
                    .read_node(ReadRequest::LogsFiltered(vibeface_client::LogFilter {
                        since_ms: parse_optional_number(&get("since_ms"))?,
                        until_ms: parse_optional_number(&get("until_ms"))?,
                        level: if get("level").is_empty() {
                            None
                        } else {
                            Some(serde_json::from_value(json!(get("level")))?)
                        },
                        sources: get("source")
                            .split(',')
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(|v| serde_json::from_value(json!(v)))
                            .collect::<std::result::Result<Vec<_>, _>>()?,
                        conversation_id: (!get("conversation_id").is_empty())
                            .then(|| get("conversation_id")),
                        work_id: (!get("work_id").is_empty()).then(|| get("work_id")),
                        text: (!get("search").is_empty()).then(|| get("search")),
                        limit: Some(500),
                    }))
                    .await?;
                let search = get("search");
                let level = get("level");
                let source = get("source");
                let rows = value["records"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|entry| {
                                let text = entry["message"].as_str()?;
                                if !text.contains(&search)
                                    || !level.is_empty() && entry["level"] != level
                                    || !source.is_empty()
                                        && !source
                                            .split(',')
                                            .map(str::trim)
                                            .any(|item| entry["source"] == item)
                                {
                                    return None;
                                }
                                Some((
                                    entry["level"].as_str().unwrap_or("info").into(),
                                    text.into(),
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let _ = self.emit(Event::Detail(rows)).await;
            }
            Editor::DecisionToken => {
                self.submit(Action::CreateDecisionToken {
                    label: get("label"),
                })
                .await?
            }
            Editor::NodeSettings => {
                let raw = self.read_node(ReadRequest::NodeSettings).await?;
                let backup = raw["settings"]["manager"]["backup"].clone();
                let manager = if get("provider").is_empty() {
                    None
                } else {
                    Some(vibeface_client::ManagerSettings {
                        provider: serde_json::from_value(json!(get("provider")))?,
                        workspace: get("workspace").into(),
                        model: get("model"),
                        reasoning_effort: (!get("reasoning_effort").is_empty())
                            .then(|| get("reasoning_effort")),
                        backup: if !values.contains_key("backup_automatic") {
                            if backup.is_null() {
                                vibeface_client::BackupPolicy::default()
                            } else {
                                serde_json::from_value(backup)?
                            }
                        } else {
                            vibeface_client::BackupPolicy {
                                automatic: parse_optional_bool(&get("backup_automatic"))?
                                    .unwrap_or(false),
                                candidates: get("backup_candidates")
                                    .lines()
                                    .filter(|line| !line.trim().is_empty())
                                    .map(|line| {
                                        let fields = line.split('|').collect::<Vec<_>>();
                                        ensure!(
                                            fields.len() == 3,
                                            "备用模型须写为 Provider|模型|推理强度"
                                        );
                                        Ok(vibeface_client::BackupModel {
                                            provider: serde_json::from_value(json!(
                                                fields[0].trim()
                                            ))?,
                                            model: fields[1].trim().into(),
                                            reasoning_effort: (!fields[2].trim().is_empty())
                                                .then(|| fields[2].trim().into()),
                                        })
                                    })
                                    .collect::<Result<Vec<_>>>()?,
                            }
                        },
                    })
                };
                self.submit(Action::NodeSettings(vibeface_client::NodeSettings {
                    roots: get("roots").lines().map(PathBuf::from).collect(),
                    manager,
                }))
                .await?;
            }
            Editor::Triage => {
                let projects = get("projects")
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| {
                        let (prefix, outlet) = line
                            .split_once('=')
                            .context("项目规则须写为 prefix=record 或 prefix=both")?;
                        Ok(vibeface_client::ProjectOutlet {
                            prefix: prefix.trim().into(),
                            outlet: serde_json::from_value(json!(outlet.trim()))?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.submit(Action::Triage(vibeface_client::TriageSettings {
                    default_outlet: serde_json::from_value(json!(get("default_outlet")))?,
                    projects,
                    webhook_url: (!get("webhook_url").is_empty()).then(|| get("webhook_url")),
                    scan_codex_sessions: get("scan_codex_sessions") == "true",
                    codex_sessions: (!get("codex_sessions").is_empty())
                        .then(|| get("codex_sessions").into()),
                    webhook_secret: if get("clear_webhook_secret") == "true" {
                        vibeface_client::SecretChange::Clear
                    } else if get("webhook_secret").is_empty() {
                        vibeface_client::SecretChange::Keep
                    } else {
                        vibeface_client::SecretChange::Set(get("webhook_secret"))
                    },
                }))
                .await?;
            }
            Editor::Provider => {
                ensure!(
                    get("device_id").is_empty()
                        || self.form_owner.as_deref().or_else(|| self
                            .active_entry
                            .as_ref()
                            .map(|entry| entry.node_id.as_str()))
                            == Some(get("device_id").as_str()),
                    "请先连接目标设备，再修改 Provider"
                );
                let provider: Provider = serde_json::from_value(json!(get("provider")))?;
                self.submit(Action::ProviderSettings(ProviderSettings {
                    provider,
                    bot: parse_optional_bool(&get("bot"))?,
                    execution: parse_optional_bool(&get("execution"))?,
                    execution_model: Some(get("model")),
                    bin: if get("bin").is_empty() {
                        None
                    } else {
                        Some(get("bin").into())
                    },
                    connect_local: get("connect_local") == "true",
                    app_server: if get("app_server_mode").is_empty() {
                        None
                    } else {
                        Some(vibeface_client::AppServer {
                            mode: serde_json::from_value(json!(get("app_server_mode")))?,
                            socket: (!get("socket").is_empty())
                                .then(|| PathBuf::from(get("socket"))),
                        })
                    },
                    env: get("env")
                        .lines()
                        .map(|line| {
                            let (key, value) =
                                line.split_once('=').context("环境变量须写为 NAME=value")?;
                            ensure!(!key.trim().is_empty(), "环境变量名称为空");
                            Ok((key.trim().to_owned(), Some(value.to_owned())))
                        })
                        .collect::<Result<BTreeMap<_, _>>>()?,
                }))
                .await?;
            }
            Editor::Relay => {
                ensure!(
                    get("device_id").is_empty()
                        || self.form_owner.as_deref().or_else(|| self
                            .active_entry
                            .as_ref()
                            .map(|entry| entry.node_id.as_str()))
                            == Some(get("device_id").as_str()),
                    "请先连接目标设备，再修改 relay"
                );
                self.submit(Action::Relay {
                    volunteer: get("volunteer") == "true",
                    listen: (!get("listen").is_empty()).then(|| get("listen")),
                    url: (!get("url").is_empty()).then(|| get("url")),
                    rate: if get("rate").is_empty() {
                        None
                    } else {
                        Some(get("rate").parse()?)
                    },
                    max_clients: if get("max_clients").is_empty() {
                        None
                    } else {
                        Some(get("max_clients").parse()?)
                    },
                    max_clients_per_ip: if get("max_clients_per_ip").is_empty() {
                        None
                    } else {
                        Some(get("max_clients_per_ip").parse()?)
                    },
                })
                .await?;
            }
            _ => unreachable!("editor routed to the wrong settings handler"),
        }
        Ok(())
    }
}
