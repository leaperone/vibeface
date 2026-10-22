//! The coordination tools of an assistant turn: one table for the schemas the SDK offers, the
//! Node's handler and the `vibeface tool` CLI.
use crate::model::ROUTINE_MINUTES;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Nodes,
    Context,
    DispatchWork,
    WorkStatus,
    InterruptWork,
    CallLookup,
    UpdateProfile,
    SaveSkill,
    RemoveSkill,
    SaveRoutine,
    RemoveRoutine,
    ReadSkill,
    TrackWork,
    BindWork,
    WaitCodex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Read,
    /// Starts or stops work; needs a live agent invocation.
    Work,
    /// Changes the assistant itself; only a turn the user's own message started gets these.
    Settings,
}

impl Tool {
    pub const ALL: [Tool; 15] = [
        Tool::Nodes,
        Tool::Context,
        Tool::DispatchWork,
        Tool::WorkStatus,
        Tool::InterruptWork,
        Tool::CallLookup,
        Tool::UpdateProfile,
        Tool::SaveSkill,
        Tool::RemoveSkill,
        Tool::SaveRoutine,
        Tool::RemoveRoutine,
        Tool::ReadSkill,
        Tool::TrackWork,
        Tool::BindWork,
        Tool::WaitCodex,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Tool::Nodes => "nodes",
            Tool::Context => "context",
            Tool::DispatchWork => "dispatch_work",
            Tool::WorkStatus => "work_status",
            Tool::InterruptWork => "interrupt_work",
            Tool::CallLookup => "call_lookup",
            Tool::UpdateProfile => "update_profile",
            Tool::SaveSkill => "save_skill",
            Tool::RemoveSkill => "remove_skill",
            Tool::SaveRoutine => "save_routine",
            Tool::RemoveRoutine => "remove_routine",
            Tool::ReadSkill => "read_skill",
            Tool::TrackWork => "track_work",
            Tool::BindWork => "bind_work",
            Tool::WaitCodex => "wait_codex",
        }
    }

    pub fn effect(self) -> Effect {
        match self {
            Tool::Nodes
            | Tool::Context
            | Tool::WorkStatus
            | Tool::CallLookup
            | Tool::ReadSkill
            | Tool::WaitCodex => Effect::Read,
            Tool::DispatchWork | Tool::InterruptWork | Tool::TrackWork | Tool::BindWork => {
                Effect::Work
            }
            Tool::UpdateProfile
            | Tool::SaveSkill
            | Tool::RemoveSkill
            | Tool::SaveRoutine
            | Tool::RemoveRoutine => Effect::Settings,
        }
    }

    /// Description, properties and required fields the agent sees. `track_work`, `bind_work` and
    /// `wait_codex` are the steps `vibeface tool dispatch_work` takes and are not offered on their own.
    pub fn schema(self) -> Option<(&'static str, Value, &'static [&'static str])> {
        let string = || json!({"type":"string","minLength":1});
        Some(match self {
            Tool::Nodes => (
                "List personal Nodes, device names and allowed roots.",
                json!({}),
                &[],
            ),
            Tool::Context => (
                "Read conversation events in their shared order, or an attachment. offset counts events, or bytes for blobs; blob limit counts KiB.",
                json!({"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":100},"blob_hash":string()}),
                &[],
            ),
            Tool::DispatchWork => (
                "Start work in cwd on target_node (default: this device). On this device the CLI registers the work with the Node, then starts one Codex turn with full access through the App Server; on another device that device's assistant gets the task. Reuse call_ref to retry the same dispatch; it never starts a second turn. parent names the work this one belongs to; continues names an earlier work this one follows up. thread sends the prompt into an existing Codex thread on this device instead, steering its running turn or starting one. model, effort and images (local image paths) pass to the turn.",
                json!({"title":{"type":"string","description":"A few words for the user, in their language, saying what this work does"},
                "prompt":{"type":"string","minLength":1,"description":"The task as Codex should read it: goal, where, limits, what done means; a required result format with a short example. Not a relay of the chat"},
                "cwd":string(),"call_ref":string(),"target_node":string(),
                "parent":string(),"continues":string(),
                "thread":{"type":"string","minLength":1,"description":"An existing Codex thread on this device: prompt goes into it, steering its running turn or starting one; cwd is taken from the thread"},
                "model":string(),
                "effort":{"type":"string","enum":["none","minimal","low","medium","high","xhigh"]},
                "images":{"type":"array","items":string(),"maxItems":8}}),
                &["title", "prompt", "cwd", "call_ref"],
            ),
            Tool::WorkStatus => (
                "Read the state and result of this conversation's works, or of one work_id.",
                json!({"work_id":string()}),
                &[],
            ),
            Tool::InterruptWork => (
                "Ask the App Server to stop a work and the works it started. It counts as stopped only when the App Server confirms.",
                json!({"work_id":string()}),
                &["work_id"],
            ),
            Tool::CallLookup => (
                "Find the work this turn dispatched, or the assistant change it made, with call_ref.",
                json!({"call_ref":string()}),
                &["call_ref"],
            ),
            Tool::UpdateProfile => (
                "Rename this assistant or replace its standing instructions because the user's own message asked. Apply it right away, without asking to confirm; the chat shows the change. instructions replaces the whole text, so keep what should stay. Reuse call_ref only to retry the same change.",
                json!({"call_ref":string(),"name":string(),"instructions":{"type":"string"}}),
                &["call_ref"],
            ),
            Tool::SaveSkill => (
                "Save a reusable skill to the library every assistant in this network shares, because the user's own message asked. Omit skill_id to add one; pass it to replace that skill. Apply it right away, without asking to confirm. body holds the complete steps.",
                json!({"call_ref":string(),"skill_id":string(),"name":string(),"description":{"type":"string"},"body":string()}),
                &["call_ref", "name", "description", "body"],
            ),
            Tool::RemoveSkill => (
                "Remove a skill from the shared library because the user's own message asked.",
                json!({"call_ref":string(),"skill_id":string()}),
                &["call_ref", "skill_id"],
            ),
            Tool::SaveRoutine => (
                "Create or replace a routine of this assistant because the user's own message asked: prompt runs as a new turn of this conversation at first_run_at_ms, then every every_minutes; missed runs are skipped. Apply it right away, without asking to confirm. Compute first_run_at_ms (Unix ms) from assistant.now_ms in the user's time zone; if they never said it, use this device's local time zone and say which one you used. Write prompt as a standalone instruction: its inputs, the expected result, its boundaries and what to do when a source is missing. In your reply restate the name, the schedule in the user's time zone, the next run and what it will do. Omit routine_id to add one; pass it to replace that routine. enabled=false pauses it.",
                json!({"call_ref":string(),"routine_id":string(),"name":string(),"prompt":string(),
                "first_run_at_ms":{"type":"integer","minimum":0},
                "every_minutes":{"type":"integer","minimum":*ROUTINE_MINUTES.start(),"maximum":*ROUTINE_MINUTES.end()},
                "enabled":{"type":"boolean"}}),
                &[
                    "call_ref",
                    "name",
                    "prompt",
                    "first_run_at_ms",
                    "every_minutes",
                ],
            ),
            Tool::RemoveRoutine => (
                "Delete a routine of this assistant because the user's own message asked.",
                json!({"call_ref":string(),"routine_id":string()}),
                &["call_ref", "routine_id"],
            ),
            Tool::ReadSkill => (
                "Read a shared skill's complete steps by skill_id.",
                json!({"skill_id":string()}),
                &["skill_id"],
            ),
            Tool::TrackWork | Tool::BindWork | Tool::WaitCodex => return None,
        })
    }

    pub fn needs_call_ref(self) -> bool {
        self.schema()
            .is_some_and(|(_, _, required)| required.contains(&"call_ref"))
    }
}
