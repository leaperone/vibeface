//! Workspace rows are disposable projections of the existing signed DAG.
mod assignment;
mod authorization;
mod command;
mod graph;
mod history_pages;
mod input;
pub(crate) mod overview;
mod query;
mod resources;

pub(crate) use assignment::{
    assignment_admission_allowed_from, assignment_work_allowed_from, assignment_work_source_from,
    assignment_work_source_matches_from, bind_waiting_workspace_inputs_from,
    freeze_workspace_input_assignment_from, task_execution_allowed_from, task_input_allowed_from,
    workspace_assignment_for_job_from, workspace_input_resources_blocked_from,
};
pub(crate) use authorization::{
    additional_dependencies, authorize_event, authorize_fresh_bot_main,
};
pub(crate) use input::{
    input_route_from, queued_workspace_input_from, resolve_input_target_from,
    workspace_input_release_allowed_from,
};
#[cfg(test)]
pub(super) use query::assert_causal_reads_match_recursive_history;
pub(crate) use query::{
    apply_projection, message_target_from, rebuild_lookup_projection, refresh_question_targets,
    refresh_reply_targets, workspace_assignment_for_work_from, workspace_assignment_from,
    workspace_task_from,
};
pub(crate) use resources::{assignment_resources_available_from, work_resources_available_from};

#[cfg(test)]
thread_local! {
    static PROJECTION_ANCESTORS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static AUTHORIZATION_ANCESTORS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FRESH_MAIN_INPUTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
pub(crate) fn projection_ancestor_walks() -> usize {
    PROJECTION_ANCESTORS.with(std::cell::Cell::get)
}
#[cfg(test)]
pub(crate) fn reset_projection_ancestor_walks() {
    PROJECTION_ANCESTORS.with(|count| count.set(0));
    AUTHORIZATION_ANCESTORS.with(|count| count.set(0));
}
#[cfg(test)]
pub(crate) fn authorization_ancestor_walks() -> usize {
    AUTHORIZATION_ANCESTORS.with(std::cell::Cell::get)
}
#[cfg(test)]
fn fresh_main_inputs() -> usize {
    FRESH_MAIN_INPUTS.with(std::cell::Cell::get)
}
#[cfg(test)]
fn reset_fresh_main_inputs() {
    FRESH_MAIN_INPUTS.with(|count| count.set(0));
}

#[cfg(test)]
#[path = "workspace_lookup_tests.rs"]
mod lookup_tests;

#[cfg(test)]
#[path = "fresh_main_input_tests.rs"]
mod fresh_main_input_tests;
