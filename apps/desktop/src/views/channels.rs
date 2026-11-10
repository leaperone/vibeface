use crate::{app::Desktop, ui::{ComposerDraft, Route}, views::{backend::Command, forms::Editor, records::{HistoryBody, RecordScope, WorkOutcome}, work_detail::outcome_label}};
use gpui_kit::{component::{button::{Button, ButtonVariants}, input::{Input,Textarea}, Disableable}, *};
use std::collections::BTreeMap;
use vibeface_protocol::workspace::{Attachment, InputControl, InputIntent, InputTarget, TaskAction, WorkspaceCommand, WorkspaceSpeaker};

impl Desktop {
    pub(crate) fn channel_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Route::Channel(channel_id) = &self.route else {return self.page().into_any_element()};
        let mut page = self.page().child(self.nav("channel-back", "所有频道", Route::Channels, cx));
        let Some(channel) = self.snapshot.channels.iter().find(|channel| channel.channel_id == *channel_id) else {
            return page.child("尚未读取此频道。连接后刷新工作区。").child(self.button("channel-reload-workspace", "刷新工作区", Command::Refresh, cx)).into_any_element();
        };
        let coordinator = self.snapshot.bots.iter().find(|bot| bot.bot_id == channel.coordinator_bot);
        let node = self.snapshot.nodes.iter().find(|node| node.device_id == channel.coordinator_node);
        page = page.child(self.row(channel.title.clone(), channel.purpose.clone()))
            .child(self.row("协调与归属", format!("{} · {} · 修订 {} · 归属版本 {}", coordinator.map(|bot|bot.name.as_str()).unwrap_or("尚未读取协调 Bot"), node.map(|node|format!("{} · {}",node.name,if node.online {"在线"} else {"离线"})).unwrap_or_else(||"设备状态未知".into()), channel.revision, channel.owner_epoch)))
            .child(self.open_editor("channel-configure", "配置频道", Editor::EditChannel(channel.channel_id.clone(),channel.revision,channel.owner_epoch),BTreeMap::from([("title",channel.title.clone()),("purpose",channel.purpose.clone()),("members",channel.members.join(",")),("coordinator_bot",channel.coordinator_bot.clone()),("coordinator_node",channel.coordinator_node.clone())]),cx))
            .child(self.button("channel-refresh-history", "重新读取频道历史", Command::History{channel_id:Some(channel_id.clone()),task_id:None},cx));
        if !self.snapshot.online {page=page.child("当前离线，显示上次读取的历史。新进度和结果尚未确认。");}
        if self.snapshot.local_session {page=page.child("当前为本机管理会话。加入个人网络后才能发送频道输入。");}
        let mut members=div().flex().flex_wrap().gap_2();
        for member in &channel.members {
            if let Some(bot)=self.snapshot.bots.iter().find(|bot|bot.bot_id==*member) {
                let target=InputTarget::ChannelMember{channel_id:channel_id.clone(),bot_id:member.clone()};
                members=members.child(self.channel_target_button(format!("member-{member}"),format!("交给 {}{}",bot.name,if *member==channel.coordinator_bot {" · 默认协调"}else{""}),target,bot.conversation_id.clone(),None,cx));
            }else{members=members.child(format!("{member} · 尚未读取 Bot"));}
        }
        page=page.child(self.row("频道成员", "普通发言只交给选中的 Bot；默认由协调 Bot 承接。")).child(members);
        let tasks=self.snapshot.tasks.iter().filter(|task|task.channel_id.as_ref()==Some(channel_id)).collect::<Vec<_>>();
        for task in &tasks {
            let task_route=Route::Task(task.task_id.clone());
            let generation=self.snapshot.generation;let network=self.snapshot.network.clone();
            let mut row=self.row(task.title.clone(),format!("{} · 第 {} 版 · 依赖 {}",task.state,task.revision,task.dependencies.join("、"))).child(task.request.clone()).child(Button::new(SharedString::from(format!("channel-open-task-{}",task.task_id))).label("打开任务详情").on_click(cx.listener(move|this,_,_,cx|{if this.same_view_scope(generation,&network){this.navigate(task_route.clone(),cx)}})));
            if let Some(bot)=self.snapshot.bots.iter().find(|bot|bot.bot_id==task.owner_bot) {
                if channel.members.contains(&bot.bot_id){row=row.child(self.channel_target_button(format!("task-{}",task.task_id),format!("回复任务 · {}",bot.name),InputTarget::ChannelTask{channel_id:channel_id.clone(),task_id:task.task_id.clone(),bot_id:bot.bot_id.clone()},bot.conversation_id.clone(),Some(task.revision),cx));}
            }
            for(label,action)in[("继续",TaskAction::Continue),("停止",TaskAction::Stop),("延后",TaskAction::Defer),("取消",TaskAction::Cancel)]{
                row=row.child(self.button(SharedString::from(format!("channel-task-{}-{label}",task.task_id)),label,Command::Workspace(WorkspaceCommand::ControlTask{task_id:task.task_id.clone(),expected_revision:task.revision,action}),cx).disabled(!self.snapshot.writable));
            }
            page=page.child(row);
        }
        let history=self.snapshot.channel_history.as_ref().filter(|history|history.channel_id.as_deref()==Some(channel_id.as_str())&&history.task_id.is_none()&&self.snapshot.network.as_deref()==Some(history.network_id.as_str()));
        if let Some(history)=history {
            page=page.child(self.row("频道历史",format!("最多 {} 条 · {} · 已核验发言身份",history.coverage.limit,if history.coverage.truncated {"历史已截断，可从任务页缩小范围后读取"}else{"已读取当前范围"})));
            let query=self.history_query.read(cx).value().as_str().trim().to_lowercase();
            let shown=history.events.iter().filter(|event|query.is_empty()||event.search_text.contains(&query)).count();
            page=page.child(Input::new(&self.history_query).aria_label("搜索频道历史"))
                .child(Button::new("clear-channel-search").label("清空搜索").on_click(cx.listener(|this,_,window,cx|{this.history_query.update(cx,|input,cx|input.set_value("",window,cx));cx.notify();})))
                .child(format!("当前读取范围匹配 {shown} / {} 条",history.events.len()));
            for event in history.events.iter().filter(|event|query.is_empty()||event.search_text.contains(&query)) {
                let speaker=match &event.speaker {
                    WorkspaceSpeaker::Human{device_id}=>format!("你 · {device_id}"),
                    WorkspaceSpeaker::Bot{bot_id}=>self.snapshot.bots.iter().find(|bot|bot.bot_id==*bot_id).map(|bot|bot.name.clone()).unwrap_or_else(||format!("Bot {bot_id}")),
                    WorkspaceSpeaker::Node{device_id}=>self.snapshot.nodes.iter().find(|node|node.device_id==*device_id).map(|node|node.name.clone()).unwrap_or_else(||format!("设备 {device_id}")),
                };
                let mut row=self.row(speaker,format!("时间 {} · 事件 {}",event.created_at_ms,event.event_hash));
                match &event.body {
                    HistoryBody::InputPosted{input}=>{
                        row=row.child(self.record_text(&format!("history:{}:body",event.event_hash),&input.text)).child(format!("{:?} · 序号 {} · 修订 {}",input.intent,input.sequence,input.expected_revision.map(|v|v.to_string()).unwrap_or_else(||"未指定".into()))).child(match input.report_results{Some(true)=>"要求报告结果",Some(false)=>"不要求报告结果",None=>"结果报告策略未提供"});
                        if let InputTarget::ChannelTask{task_id,bot_id,..}=&input.target {
                            if let Some(task)=tasks.iter().find(|task|task.task_id==*task_id&&task.owner_bot==*bot_id) {
                                if let Some(bot)=self.snapshot.bots.iter().find(|bot|bot.bot_id==*bot_id&&bot.conversation_id==event.scope_id) {
                                    row=row.child(self.channel_target_button(format!("reply-{}",event.event_hash),"回复这个任务".into(),input.target.clone(),bot.conversation_id.clone(),Some(task.revision),cx));
                                }
                            }
                        }
                    }
                    HistoryBody::MessagePosted{message_id,role,text,in_reply_to,status,failure,..}=>{
                        row=row.child(self.record_text(&format!("history:{}:body",event.event_hash),text)).child(format!("{:?} · 消息 {}{}",role,message_id,status.map(|s|format!(" · {s:?}")).unwrap_or_default()));
                        if let Some(reply)=in_reply_to {row=row.child(format!("回复消息 {reply}"));}
                        if let Some(failure)=failure {row=row.child(format!("执行失败类型 {failure:?}"));}
                    }
                    HistoryBody::WorkDispatched{work_id,title,prompt,kind,model,resolved,cwd,..}=>{
                        row=row.child(format!("接受工作 · {title}")).child(self.record_text(&format!("history:{}:body",event.event_hash),prompt)).child(format!("类型 {kind:?} · 请求模型 {} · 接收 {:?} / {} · {cwd}",model.as_deref().unwrap_or("设备默认模型"),resolved.provider,resolved.model.as_deref().unwrap_or("设备默认模型"))).child(self.history_work_button(&event.scope_id,work_id,cx));
                    }
                    HistoryBody::WorkProgress{work_id,phase,summary,thread_id,turn_id}=>{
                        row=row.child(format!("工作进度 · {phase:?}")).child(self.record_text(&format!("history:{}:body",event.event_hash),summary)).child(format!("线程 {} · 运行 {}",thread_id.as_deref().unwrap_or("未上报"),turn_id.as_deref().unwrap_or("未上报"))).child(self.history_work_button(&event.scope_id,work_id,cx));
                    }
                    HistoryBody::WorkFinished{work_id,outcome,result_text,failure_reason,failure}=>{
                        row=row.child(format!("工作结果 · {}",outcome_label(*outcome))).child(self.record_text(&format!("history:{}:body",event.event_hash),result_text)).child(self.history_work_button(&event.scope_id,work_id,cx));
                        if let Some(reason)=failure_reason {row=row.child(reason.clone());}
                        if let Some(failure)=failure {row=row.child(format!("执行失败类型 {failure:?}"));}
                        if *outcome==WorkOutcome::Unverified {row=row.child("尚未证明实际结果，先核实原执行记录。");}
                    }
                    HistoryBody::WorkspaceCommanded{command_id,command}=>{
                        row=row.child(format!("共享操作 {command_id}"));
                        match command {
                            WorkspaceCommand::PostChannelMessage{message_id,text,recipient_bot,task_id,reply_to,..}=>{
                                row=row.child(self.record_text(&format!("history:{}:body",event.event_hash),text)).child(format!("接收 Bot {} · 任务 {} · 回复 {}",recipient_bot.as_deref().unwrap_or(&channel.coordinator_bot),task_id.as_deref().unwrap_or("未指定"),reply_to.as_deref().unwrap_or("未指定")));
                                row=row.child(self.open_editor(SharedString::from(format!("reply-post-{}",event.event_hash)),"回复此频道消息",Editor::ChannelPost(channel_id.clone()),BTreeMap::from([("recipient_bot",recipient_bot.clone().unwrap_or_else(||channel.coordinator_bot.clone())),("task_id",task_id.clone().unwrap_or_default()),("reply_to",message_id.clone())]),cx));
                            }
                            WorkspaceCommand::CreateTask{title,request,expected_result,dependencies,..}=>{row=row.child(format!("创建任务 · {title}")).child(request.clone()).child(format!("预期结果 {expected_result}\n依赖 {}",dependencies.join("、")));}
                            WorkspaceCommand::ReviseTask{expected_revision,request,expected_result,..}=>{row=row.child(format!("修改任务交代 · 预期第 {expected_revision} 版")).child(request.clone()).child(expected_result.clone());}
                            WorkspaceCommand::ControlTask{action,expected_revision,..}=>{row=row.child(format!("任务控制 {action:?} · 预期第 {expected_revision} 版"));}
                            WorkspaceCommand::AssignTask{bot_id,execution_node,expected_revision,..}=>{row=row.child(format!("分派执行 · Bot {bot_id} · 设备 {execution_node} · 第 {expected_revision} 版"));}
                            WorkspaceCommand::AssignmentProgress{revision,state,result,..}=>{row=row.child(format!("执行声明 · 第 {revision} 版 · {state:?}。实际完成状态以任务记录为准。"));if let Some(result)=result {row=row.child(self.record_text(&format!("history:{}:body",event.event_hash),result));}}
                            WorkspaceCommand::ControlInput{message_id,action,..}=>{row=row.child(format!("输入控制 · {message_id}"));match action{InputControl::Release=>row=row.child("释放延后输入"),InputControl::Cancel=>row=row.child("取消输入"),InputControl::Move{before_message_id}=>row=row.child(before_message_id.as_ref().map(|id|format!("移到输入 {id} 之前")).unwrap_or_else(||"移到输入队列末尾".into())),InputControl::Edit{text,..}=>row=row.child("修改输入内容").child(text.clone())};}
                            WorkspaceCommand::CreateChannel{title,purpose,members,..}|WorkspaceCommand::ConfigureChannel{title,purpose,members,..}=>{row=row.child(format!("频道配置 · {title} · {purpose} · {} 位成员",members.len()));}
                        }
                    }
                }
                if let Some(conversation)=&event.attachment_conversation {for file in event.body.attachments(){row=row.child(self.record_attachment(&event.event_hash,conversation,file,cx));}}
                page=page.child(row);
            }
            if history.events.is_empty(){page=page.child("当前读取范围没有历史事件。");}
            else if shown==0 {page=page.child("当前读取范围没有匹配的历史事件。搜索范围不包含被截断的历史。");}
        }else{page=page.child("尚未读取当前频道的历史；不会使用其他频道的缓存。");}
        for question in self.snapshot.questions.iter().filter(|q|q.answer.is_none()&&q.closed.is_none()&&match &q.target{Some(InputTarget::ChannelMember{channel_id:id,..}|InputTarget::ChannelTask{channel_id:id,..})=>id==channel_id,_=>false}){
            let mut row=self.row("频道任务需要你回复",question.text.clone());
            for choice in &question.choices{row=row.child(self.button(SharedString::from(format!("channel-answer-{}-{}",question.question_id,choice.label)),choice.label.clone(),Command::Answer{conversation_id:question.conversation_id.clone(),question_id:question.question_id.clone(),text:choice.label.clone()},cx).disabled(!self.snapshot.writable));}
            page=page.child(row.child(self.open_editor(SharedString::from(format!("channel-answer-text-{}",question.question_id)),"输入回复",Editor::Answer(question.question_id.clone(),question.conversation_id.clone()),BTreeMap::new(),cx)));
        }
        for queued in self.snapshot.inputs.iter().filter(|queued|!queued.cancelled&&match &queued.input.route.target{InputTarget::ChannelMember{channel_id:id,..}|InputTarget::ChannelTask{channel_id:id,..}=>id==channel_id,_=>false}){
            let mut row=self.row(if queued.deferred{"已延后频道输入"}else{"等待执行的频道输入"},queued.input.text.clone());
            for(label,action)in[("释放",InputControl::Release),("取消",InputControl::Cancel),("移到末尾",InputControl::Move{before_message_id:None})]{row=row.child(self.button(SharedString::from(format!("channel-queued-{}-{label}",queued.input.message_id)),label,Command::Workspace(WorkspaceCommand::ControlInput{message_id:queued.input.message_id.clone(),expected_event:queued.control_event.clone(),action}),cx).disabled(!self.snapshot.writable));}
            page=page.child(row.child(self.open_editor(SharedString::from(format!("channel-queued-edit-{}",queued.input.message_id)),"编辑输入",Editor::QueuedEdit(queued.clone()),BTreeMap::from([("text",queued.input.text.clone())]),cx)));
            for file in &queued.input.attachments{page=page.child(self.record_attachment(&queued.input.message_id,&queued.conversation_id,file,cx));}
        }
        let selected=self.selected_target.as_ref().filter(|target|match target{InputTarget::ChannelMember{channel_id:id,..}|InputTarget::ChannelTask{channel_id:id,..}=>id==channel_id,_=>false});
        let target=selected.cloned().or_else(||coordinator.map(|bot|InputTarget::ChannelMember{channel_id:channel_id.clone(),bot_id:bot.bot_id.clone()}));
        if let Some(target)=target {
            let (bot_id,revision)=match &target {InputTarget::ChannelMember{bot_id,..}=>(bot_id,None),InputTarget::ChannelTask{bot_id,task_id,..}=>(bot_id,tasks.iter().find(|task|task.task_id==*task_id).map(|task|task.revision)),_=>unreachable!()};
            if matches!(&target,InputTarget::ChannelTask{..})&&revision.is_none(){page=page.child("尚未读取选中的频道任务，刷新工作区后再发送。");}
            else if let Some(bot)=self.snapshot.bots.iter().find(|bot|bot.bot_id==*bot_id&&channel.members.contains(bot_id)) {
                page=page.child(self.row(format!("发送给 {}",bot.name),if revision.is_some(){"回复所选频道任务"}else{"创建新的频道交代"})).child(self.record_composer(bot.conversation_id.clone(),target,revision,true,cx));
            }
        }
        page.child(self.record_artifact_preview(cx)).into_any_element()
    }

    fn channel_target_button(&self,id:String,label:String,target:InputTarget,conversation_id:String,revision:Option<u64>,cx:&mut Context<Self>)->Button{
        let rendered=RecordScope{generation:self.snapshot.generation,network_id:self.snapshot.network.clone()};
        Button::new(SharedString::from(id)).label(label).on_click(cx.listener(move|this,_,_,cx|{
            if !rendered.accepts(this.snapshot.generation,this.snapshot.network.as_deref()){return}
            this.selected=Some(conversation_id.clone());this.selected_target=Some(target.clone());this.target_revision=revision;this.intent=InputIntent::Continue;
            let scope=record_attachment_scope(&conversation_id,&target);this.attachment_scope=Some(scope.clone());this.attachments.clear();
            this.target_kind=if matches!(&target,InputTarget::ChannelTask{..}){5}else{4};
            this.send(Command::SelectAttachmentScope(scope),cx);cx.notify();
        }))
    }
    fn history_work_button(&self,conversation:&str,work:&str,cx:&mut Context<Self>)->Button{
        let route=Route::Work{conversation_id:conversation.into(),work_id:work.into()};
        let rendered=RecordScope{generation:self.snapshot.generation,network_id:self.snapshot.network.clone()};
        Button::new(SharedString::from(format!("history-work-{conversation}-{work}"))).label("打开工作记录").on_click(cx.listener(move|this,_,_,cx|{if rendered.accepts(this.snapshot.generation,this.snapshot.network.as_deref()){this.navigate(route.clone(),cx)}}))
    }

    pub(crate) fn record_attachment(&self,key:&str,conversation:&str,file:&Attachment,cx:&mut Context<Self>)->AnyElement{
        let save=file.clone();let scope=conversation.to_owned();
        let rendered=RecordScope{generation:self.snapshot.generation,network_id:self.snapshot.network.clone()};
        div().flex().flex_col().gap_2().child(format!("{} · {} 字节 · SHA-256 {}",file.name,file.size,file.hash))
            .child(self.button(SharedString::from(format!("record-preview-{key}-{}",file.hash)),"预览文本",Command::ArtifactPreview{conversation_id:conversation.into(),file:file.clone(),offset:0},cx).disabled(!self.snapshot.online))
            .child(Button::new(SharedString::from(format!("record-save-{key}-{}",file.hash))).label("保存附件").disabled(!self.snapshot.online).on_click(cx.listener(move|this,_,_,cx|{if rendered.accepts(this.snapshot.generation,this.snapshot.network.as_deref()){this.save_artifact(scope.clone(),save.clone(),cx)}}))).into_any_element()
    }

    pub(crate) fn record_artifact_preview(&self,cx:&mut Context<Self>)->AnyElement{
        let Some(page)=self.snapshot.artifact_preview.as_ref().filter(|page|{
            self.snapshot.network.as_deref()==Some(page.network_id.as_str()) && match &self.route {
                Route::Work{conversation_id,work_id}=>*conversation_id==page.conversation_id&&(
                    self.snapshot.inputs.iter().any(|input|input.conversation_id==*conversation_id&&matches!(&input.input.route.target,InputTarget::Work{work_id:id} if id==work_id)&&input.input.attachments.contains(&page.file))
                    || self.snapshot.work_detail.as_ref().is_some_and(|detail|detail.work.work_id==*work_id&&self.snapshot.channel_history.as_ref().is_some_and(|history|history.network_id==page.network_id&&history.events.iter().any(|event|event.scope_id==*conversation_id&&matches!(&event.body,HistoryBody::InputPosted{input} if detail.work.target.as_ref()==Some(&input.target)&&input.attachments.contains(&page.file)))) )
                ),
                Route::Channel(channel_id)=>self.snapshot.channel_history.as_ref().is_some_and(|history|history.channel_id.as_ref()==Some(channel_id)&&history.network_id==page.network_id&&history.events.iter().any(|event|event.attachment_conversation.as_ref()==Some(&page.conversation_id)&&event.body.attachments().contains(&page.file)))
                    || self.snapshot.inputs.iter().any(|input|input.conversation_id==page.conversation_id&&input.input.attachments.contains(&page.file)&&matches!(&input.input.route.target,InputTarget::ChannelMember{channel_id:id,..}|InputTarget::ChannelTask{channel_id:id,..} if id==channel_id)),
                Route::Task(task_id)=>self.snapshot.channel_history.as_ref().is_some_and(|history|history.task_id.as_ref()==Some(task_id)&&history.network_id==page.network_id&&history.events.iter().any(|event|event.attachment_conversation.as_ref()==Some(&page.conversation_id)&&event.body.attachments().contains(&page.file))),
                _=>false,
            }
        })else{return div().into_any_element()};
        let mut body=self.row(format!("附件预览 · {}",page.file.name),format!("原始对话 {} · 字节偏移 {} / {}",page.conversation_id,page.offset,page.file.size))
            .child(self.record_text(&format!("artifact:{}:{}",page.file.hash,page.offset),&page.text));
        if page.offset>0{body=body.child(self.button("artifact-first-page","回到第一页",Command::ArtifactPreview{conversation_id:page.conversation_id.clone(),file:page.file.clone(),offset:0},cx));}
        if let Some(offset)=page.next_offset{body=body.child(self.button("artifact-next-page","下一页",Command::ArtifactPreview{conversation_id:page.conversation_id.clone(),file:page.file.clone(),offset},cx));}else{body=body.child("已读取到文件末尾。");}
        body.into_any_element()
    }

    pub(crate) fn record_composer(&self,conversation_id:String,target:InputTarget,revision:Option<u64>,allow_continue:bool,cx:&mut Context<Self>)->AnyElement{
        let attachment_scope=record_attachment_scope(&conversation_id,&target);
        let mut body=div().flex().flex_col().gap_2().child(self.row("补充与新交代","消息先保存到本机耐久队列；未知结果需核对原操作。"));
        let mut intents=div().flex().flex_wrap().gap_2();
        for(label,intent)in[("继续",InputIntent::Continue),("新任务",InputIntent::NewTask),("延后",InputIntent::Defer)]{
            intents=intents.child(Button::new(SharedString::from(format!("record-intent-{label}"))).label(if self.intent==intent{format!("已选：{label}")}else{label.into()}).disabled(!allow_continue&&intent!=InputIntent::NewTask).on_click(cx.listener(move|this,_,_,cx|{this.intent=intent;cx.notify();})));
        }
        body=body.child(intents).child(Textarea::new(&self.composer).h(px(110.)).aria_label("发送到当前工作或频道的内容"));
        for file in self.attachments.iter().filter(|_|self.attachment_scope.as_ref()==Some(&attachment_scope)){
            let scope=attachment_scope.clone();let hash=file.hash.clone();
            let rendered=RecordScope{generation:self.snapshot.generation,network_id:self.snapshot.network.clone()};
            body=body.child(div().flex().gap_2().child(format!("{} · {} 字节",file.name,file.size)).child(Button::new(SharedString::from(format!("record-release-{}",file.hash))).label("移除附件").ghost().on_click(cx.listener(move|this,_,_,cx|{if rendered.accepts(this.snapshot.generation,this.snapshot.network.as_deref()){this.send(Command::ReleaseAttachment{conversation_id:scope.clone(),hash:hash.clone()},cx)}}))));
        }
        let scope=attachment_scope.clone();let input_conversation=conversation_id.clone();let input_target=target.clone();
        let add_scope=RecordScope{generation:self.snapshot.generation,network_id:self.snapshot.network.clone()};
        let send_scope=add_scope.clone();
        body=body.child(Button::new("record-add-attachment").label("添加附件").disabled(!self.snapshot.can_stage_input()).on_click(cx.listener(move|this,_,window,cx|{if !add_scope.accepts(this.snapshot.generation,this.snapshot.network.as_deref()){return}this.selected=Some(input_conversation.clone());this.selected_target=Some(input_target.clone());if this.attachment_scope.as_ref()!=Some(&scope){this.attachments.clear();this.attachment_scope=Some(scope.clone());this.send(Command::SelectAttachmentScope(scope.clone()),cx);}this.pick_attachment(window,cx);})))
            .child(Button::new("record-send").label(if self.sending{"正在保存输入"}else if !self.snapshot.online{"保存待发，等待连接"}else if !self.snapshot.writable{"保存待发，等待可写状态"}else{"保存并发送"}).primary().disabled(self.sending||!self.snapshot.can_stage_input()||(!allow_continue&&self.intent!=InputIntent::NewTask)).on_click(cx.listener(move|this,_,_,cx|{
                if !send_scope.accepts(this.snapshot.generation,this.snapshot.network.as_deref()){return}
                let draft=ComposerDraft{target:target.clone(),intent:this.intent,text:this.composer.read(cx).value().to_string(),attachments:if this.attachment_scope.as_ref()==Some(&attachment_scope){this.attachments.clone()}else{vec![]},expected_revision:revision};
                if let Err(error)=draft.validate(){this.notice=Some(error);cx.notify();return}
                this.sending=true;this.send(Command::Input{conversation_id:conversation_id.clone(),draft},cx);
            })));
        if !allow_continue&&self.intent!=InputIntent::NewTask{body=body.child("此工作当前不能接收补充。选择「新任务」继续。");}
        body.into_any_element()
    }
}

pub(crate) fn record_attachment_scope(conversation:&str,target:&InputTarget)->String{
    match target{
        InputTarget::Work{work_id}=>format!("work:{conversation}:{work_id}"),
        InputTarget::ChannelMember{channel_id,bot_id}=>format!("channel:{channel_id}:member:{bot_id}"),
        InputTarget::ChannelTask{channel_id,task_id,bot_id}=>format!("channel:{channel_id}:task:{task_id}:{bot_id}"),
        _=>conversation.into(),
    }
}
