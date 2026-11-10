use super::*;

impl Backend {
    pub(super) async fn save_bot(
        &mut self,
        editor: Editor,
        values: BTreeMap<String, String>,
        conversation_id: Option<String>,
    ) -> Result<()> {
        #[cfg(test)]
        Self::test_phase("save-bot-enter");
        let get = |key: &str| values.get(key).cloned().unwrap_or_default();
        let id = || conversation_id.clone().context("请先打开一个 Bot 的对话");
        let new_id = || {
            if get("_submission_id").is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                get("_submission_id")
            }
        };
        match editor {
            Editor::CreateBot => {
                ensure!(!get("name").trim().is_empty(), "请输入 Bot 名称");
                let project = if get("cwd").is_empty() {
                    None
                } else {
                    Some(vibeface_client::BotProject {
                        target_node: get("target_node"),
                        cwd: get("cwd"),
                    })
                };
                let scheduler_node = self
                    .active_entry
                    .as_ref()
                    .context("当前 Node 未确认")?
                    .node_id
                    .clone();
                self.submit(Action::CreateBot {
                    bot_id: format!("b-{}", new_id()),
                    conversation_id: format!("c-{}", new_id()),
                    name: get("name"),
                    instructions: get("instructions"),
                    scheduler_node,
                    project,
                })
                .await?;
            }
            Editor::EditBot => {
                let conversation_id = id()?;
                let bot = self
                    .state
                    .bots
                    .iter()
                    .find(|b| b.conversation_id == conversation_id)
                    .context("Bot 未加载")?;
                let bot_id = bot.bot_id.clone();
                let instruction = format!(
                    "请更新你的配置：名称 {}；指令 {}。保留没有提到的模型、推理强度、技能、头像与计划。",
                    get("name"),
                    get("instructions")
                );
                let project = if get("cwd").is_empty() {
                    None
                } else {
                    Some(vibeface_client::BotProject {
                        target_node: get("target_node"),
                        cwd: get("cwd"),
                    })
                };
                self.submit(Action::SetBotProject {
                    conversation_id: conversation_id.clone(),
                    project,
                })
                .await?;
                self.input_instruction(conversation_id, bot_id, instruction)
                    .await?;
            }
            Editor::BotModel(provider) => {
                let conversation_id = id()?;
                let bot_id = self
                    .state
                    .bots
                    .iter()
                    .find(|bot| bot.conversation_id == conversation_id)
                    .context("Bot尚未读取")?
                    .bot_id
                    .clone();
                let provider = serde_json::to_value(provider)?
                    .as_str()
                    .context("Provider名称无效")?
                    .to_owned();
                self.input_instruction(conversation_id,bot_id,format!("请仅修改你在 {provider} 上的模型偏好为「{}」，推理强度为「{}」。保留其他Provider与其他配置。",if get("model").is_empty(){"设备默认模型".into()}else{get("model")},if get("effort").is_empty(){"由Provider控制".into()}else{get("effort")})).await?;
            }
            Editor::Memory(memory) => {
                let (memory_id, base) = if let Some(memory) = memory {
                    (memory.memory_id, memory_base(&memory.metadata)?)
                } else {
                    (
                        new_id(),
                        MemoryBase {
                            epoch: 1,
                            version: None,
                        },
                    )
                };
                self.submit(Action::Memory {
                    conversation_id: id()?,
                    memory_id,
                    base,
                    after: Some(get("text")),
                })
                .await?;
            }
            Editor::EditSchedule(schedule) => {
                let conversation_id = id()?;
                let bot_id = self
                    .state
                    .bots
                    .iter()
                    .find(|b| b.conversation_id == conversation_id)
                    .context("Bot未加载")?
                    .bot_id
                    .clone();
                let time = get("first_run_at_ms").parse::<i64>()?;
                let interval = get("every_minutes");
                if !interval.is_empty() {
                    ensure!(interval.parse::<u64>()? > 0, "间隔须大于零");
                }
                self.input_instruction(
                    conversation_id,
                    bot_id,
                    format!(
                        "请修改计划 {}：名称 {}；指令 {}；首次执行时间 Unix 毫秒 {}；间隔分钟 {}。",
                        schedule.schedule_id,
                        get("name"),
                        get("prompt"),
                        time,
                        if interval.is_empty() {
                            "仅执行一次"
                        } else {
                            &interval
                        }
                    ),
                )
                .await?;
            }
            Editor::Schedule => {
                let conversation_id = id()?;
                let bot_id = self
                    .state
                    .bots
                    .iter()
                    .find(|b| b.conversation_id == conversation_id)
                    .context("Bot 未加载")?
                    .bot_id
                    .clone();
                let time = get("first_run_at_ms")
                    .parse::<i64>()
                    .context("首次执行时间须为 Unix 毫秒")?;
                let interval = get("every_minutes");
                if !interval.is_empty() {
                    ensure!(interval.parse::<u64>()? > 0, "间隔分钟须大于零");
                }
                let instruction = format!(
                    "请创建计划：名称 {}；指令 {}；首次执行时间 Unix 毫秒 {}；间隔分钟 {}。",
                    get("name"),
                    get("prompt"),
                    time,
                    if interval.is_empty() {
                        "仅执行一次"
                    } else {
                        &interval
                    }
                );
                self.input_instruction(conversation_id, bot_id, instruction)
                    .await?;
            }
            Editor::Avatar => {
                let conversation_id = id()?;
                let hue = get("hue").parse::<u16>()?;
                ensure!(
                    [20, 45, 100, 130, 155, 180, 205, 230, 255, 285, 315, 345].contains(&hue),
                    "请选择已有头像色相"
                );
                self.submit(Action::SetBotAvatar {
                    conversation_id: conversation_id.clone(),
                    version: self
                        .state
                        .bots
                        .iter()
                        .find(|bot| bot.conversation_id == conversation_id)
                        .and_then(|bot| bot.avatar_version.clone()),
                    avatar: Some(vibeface_client::Avatar {
                        outline: serde_json::from_value(json!(get("outline")))?,
                        eyes: serde_json::from_value(json!(get("eyes")))?,
                        hue,
                    }),
                })
                .await?;
            }
            _ => unreachable!("editor routed to the wrong bot handler"),
        }
        Ok(())
    }
}
