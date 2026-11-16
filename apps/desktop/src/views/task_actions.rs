use super::{backend::Command, forms::Editor};
use crate::ui::Task;
use std::collections::BTreeMap;
use vibeface_protocol::workspace::{TaskAction, WorkspaceCommand};

pub(super) fn task_editor(task: &Task) -> (Editor, BTreeMap<&'static str, String>) {
    (
        Editor::EditTask(task.task_id.clone(), task.revision),
        BTreeMap::from([
            ("title", task.title.clone()),
            ("owner_bot", task.owner_bot.clone()),
            ("request", task.request.clone()),
            ("expected_result", task.expected_result.clone()),
            ("dependencies", task.dependencies.join(",")),
        ]),
    )
}

pub(super) fn task_controls(task: &Task) -> [(&'static str, Command); 4] {
    [
        ("继续", TaskAction::Continue),
        ("停止", TaskAction::Stop),
        ("延后", TaskAction::Defer),
        ("取消", TaskAction::Cancel),
    ]
    .map(|(label, action)| {
        (
            label,
            Command::Workspace(WorkspaceCommand::ControlTask {
                task_id: task.task_id.clone(),
                expected_revision: task.revision,
                action,
            }),
        )
    })
}
