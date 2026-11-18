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
mod validation;

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
pub(crate) use validation::{validate_attachments, validate_command, validate_target};

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
mod root_path_tests {
    use super::validation::{overlap, valid_roots};

    #[test]
    fn accepts_normalized_absolute_paths_for_each_platform() {
        for path in [
            "/srv/work",
            "/srv/项目",
            "C:\\Users\\dev\\work",
            "C:\\资料\\项目",
            "D:/work",
            "\\\\server\\share\\work",
            "\\\\?\\C:\\work",
            "\\\\?\\C:\\work.",
            "\\\\?\\C:\\work ",
            "\\\\?\\UNC\\server\\share\\work",
        ] {
            assert!(valid_roots(&[path.to_owned()]).is_ok(), "{path}");
        }
    }

    #[test]
    fn rejects_relative_and_noncanonical_paths_for_each_platform() {
        for path in [
            "relative/work",
            "/srv/./work",
            "/srv/../work",
            "/srv//work",
            "/srv/work/",
            "C:work",
            "C:\\work\\..\\other",
            "C:\\work/../other",
            "C:/work\\..\\other",
            "C:\\work\\",
            "C:\\work.",
            "C:\\work ",
            "C:/work./repo",
            "\\\\.\\share\\work",
            "\\\\server\\..\\work",
            "\\\\server\\share\\..\\work",
            "\\\\server\\share\\work\\\\x",
            "\\\\server\\share\\work.",
            "\\\\server.\\share\\work",
            "/srv/wo\u{7}rk",
        ] {
            assert!(valid_roots(&[path.to_owned()]).is_err(), "{path:?}");
        }
    }

    #[test]
    fn overlap_uses_portable_components() {
        assert!(overlap("C:\\Work\\repo", "c:\\work"));
        assert!(overlap("\\\\?\\C:\\Work\\repo", "C:/work"));
        assert!(!overlap("\\\\?\\C:\\work.", "C:\\work"));
        assert!(!overlap("\\\\?\\C:\\work ", "C:\\work"));
        assert!(!overlap("C:\\Work", "D:\\Work"));
        assert!(overlap("/srv/work", "/srv/work/repo"));
        assert!(!overlap("/srv/work", "/srv/worker"));
        assert!(overlap("\\\\server\\share\\work", "\\\\server\\share"));
        assert!(overlap(
            "\\\\?\\UNC\\server\\share\\work",
            "\\\\server\\share"
        ));
        assert!(!overlap(
            "\\\\server\\share\\work",
            "\\\\other\\share\\work"
        ));
    }

    #[test]
    fn windows_unicode_case_aliases_overlap_without_normalization_or_expansion() {
        assert!(overlap("C:\\Ä\\repo", "c:\\ä"));
        assert!(overlap("\\\\SÉRVER\\SHÄRE\\repo", "\\\\sérver\\shäre"));
        assert!(overlap("\\\\?\\C:\\Ä\\repo", "C:\\ä"));
        assert!(overlap("C:\\Σ\\repo", "c:\\ς"));

        assert!(!overlap("C:\\Straße", "c:\\STRASSE"));
        assert!(!overlap("C:\\Ä", "C:\\A\u{0308}"));
        assert!(!overlap("/srv/Ä", "/srv/ä"));
        assert!(!overlap("C:\\ä", "C:\\ä-repo"));
    }
}

#[cfg(test)]
#[path = "workspace_lookup_tests.rs"]
mod lookup_tests;

#[cfg(test)]
#[path = "fresh_main_input_tests.rs"]
mod fresh_main_input_tests;
