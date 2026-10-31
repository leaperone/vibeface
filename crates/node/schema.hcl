table "networks" {
  schema = schema.main
  column "network_id" {
    null = true
    type = text
  }
  column "bundle" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id]
  }
}
table "events" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "author" {
    null = false
    type = text
  }
  column "seq" {
    null = false
    type = integer
  }
  column "hash" {
    null = false
    type = text
  }
  column "scope_id" {
    null = false
    type = text
  }
  column "lamport" {
    null = false
    type = integer
  }
  column "created_at_ms" {
    null = false
    type = integer
  }
  column "seen_at_ms" {
    null = false
    type = integer
  }
  column "envelope" {
    null = false
    type = text
  }
  column "main" {
    null = false
    type = integer
  }
  column "effective" {
    null = false
    type = integer
  }
  column "kind" {
    null = false
    type = text
  }
  column "subject" {
    null = true
    type = text
  }
  primary_key {
    columns = [column.network_id, column.hash]
  }
  foreign_key "0" {
    columns     = [column.network_id]
    ref_columns = [table.networks.column.network_id]
    on_update   = NO_ACTION
    on_delete   = NO_ACTION
  }
  index "events_main" {
    unique  = true
    columns = [column.network_id, column.author, column.seq]
    where   = "main = 1"
  }
  index "events_forks" {
    columns = [column.network_id, column.author, column.seq]
    where   = "main = 0"
  }
  index "events_order" {
    columns = [column.network_id, column.scope_id, column.lamport, column.created_at_ms, column.hash]
  }
  index "events_scope_author" {
    columns = [column.network_id, column.scope_id, column.author, column.seq]
  }
  index "events_kind" {
    columns = [column.network_id, column.scope_id, column.kind, column.subject]
  }
}
table "event_blobs" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "hash" {
    null = false
    type = text
  }
  column "blob" {
    null = false
    type = text
  }
  column "size" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.network_id, column.hash, column.blob]
  }
  index "event_blobs_blob" {
    columns = [column.network_id, column.blob]
  }
}
table "log_heads" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "author" {
    null = false
    type = text
  }
  column "max_seq" {
    null = false
    type = integer
  }
  column "head_hash" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.author]
  }
}
table "pending_events" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "hash" {
    null = false
    type = text
  }
  column "envelope" {
    null = false
    type = text
  }
  column "missing" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.hash]
  }
  index "pending_events_missing" {
    columns = [column.network_id, column.missing]
  }
}
table "conversations" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "title" {
    null = true
    type = text
  }
  column "title_rank" {
    null = true
    type = text
  }
  column "owner" {
    null = true
    type = text
  }
  column "owner_epoch" {
    null    = false
    type    = integer
    default = 0
  }
  column "owner_key" {
    null    = false
    type    = text
    default = ""
  }
  primary_key {
    columns = [column.network_id, column.conversation_id]
  }
}
table "messages" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "message_id" {
    null = false
    type = text
  }
  column "event_hash" {
    null = false
    type = text
  }
  column "rank" {
    null = false
    type = text
  }
  column "author" {
    null = false
    type = text
  }
  column "role" {
    null = false
    type = text
  }
  column "text" {
    null = false
    type = text
  }
  column "created_at_ms" {
    null = false
    type = integer
  }
  column "in_reply_to" {
    null = true
    type = text
  }
  column "status" {
    null = true
    type = text
  }
  column "report_results" {
    null = true
    type = integer
  }
  column "answers" {
    null = true
    type = text
  }
  column "attachments" {
    null = false
    type = text
  }
  column "sender" {
    null = true
    type = text
  }
  column "steered_into" {
    null = true
    type = text
  }
  column "failure" {
    null = true
    type = text
  }
  primary_key {
    columns = [column.network_id, column.conversation_id, column.message_id]
  }
  index "messages_order" {
    columns = [column.network_id, column.conversation_id, column.rank]
  }
  index "messages_answers" {
    columns = [column.network_id, column.answers]
  }
}
table "bots" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "bot_id" {
    null = false
    type = text
  }
  column "name" {
    null = false
    type = text
  }
  column "instructions" {
    null = false
    type = text
  }
  column "project" {
    null = true
    type = text
  }
  column "event_hash" {
    null = false
    type = text
  }
  column "rank" {
    null = false
    type = text
  }
  column "cause" {
    null = true
    type = text
  }
  primary_key {
    columns = [column.network_id, column.conversation_id]
  }
}
table "bot_profiles" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "field" {
    null = false
    type = text
  }
  column "value" {
    null = false
    type = text
  }
  column "rank" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.conversation_id, column.field]
  }
}
table "bot_models" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "provider" {
    null = false
    type = text
  }
  column "model" {
    null = false
    type = text
  }
  column "rank" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.conversation_id, column.provider]
  }
}
table "bot_deletions" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.conversation_id]
  }
}
table "skills" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "skill_id" {
    null = false
    type = text
  }
  column "skill" {
    null = true
    type = text
  }
  column "rank" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.skill_id]
  }
}
table "schedules" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "schedule_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "schedule" {
    null = true
    type = text
  }
  column "rank" {
    null = false
    type = text
  }
  column "changed_at_ms" {
    null = false
    type = integer
  }
  column "enabled" {
    null = false
    type = integer
  }
  column "enabled_rank" {
    null = false
    type = text
  }
  column "enabled_at_ms" {
    null = false
    type = integer
  }
  column "removed" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.network_id, column.schedule_id]
  }
}
table "memories" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "memory_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "text" {
    null = true
    type = text
  }
  column "rank" {
    null = false
    type = text
  }
  column "changed_at_ms" {
    null = false
    type = integer
  }
  column "epoch" {
    null = false
    type = integer
  }
  column "revision" {
    null = false
    type = text
  }
  column "origin" {
    null = false
    type = text
  }
  column "user_controlled" {
    null = false
    type = integer
  }
  column "essential" {
    null = false
    type = integer
  }
  column "source_event_hash" {
    null = true
    type = text
  }
  primary_key {
    columns = [column.network_id, column.memory_id]
  }
  index "memories_conversation" {
    columns = [column.network_id, column.conversation_id]
  }
}
table "bot_changes" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "change_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "event_hash" {
    null = false
    type = text
  }
  column "rank" {
    null = false
    type = text
  }
  column "cause" {
    null = true
    type = text
  }
  column "change" {
    null = false
    type = text
  }
  column "created_at_ms" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.network_id, column.change_id]
  }
  index "bot_changes_order" {
    columns = [column.network_id, column.conversation_id, column.rank]
  }
}
table "works" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "work_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "target_node" {
    null = true
    type = text
  }
  column "dispatch" {
    null = true
    type = text
  }
  column "dispatch_rank" {
    null = true
    type = text
  }
  column "progress" {
    null = true
    type = text
  }
  column "progress_rank" {
    null = true
    type = text
  }
  column "thread_id" {
    null = true
    type = text
  }
  column "turn_id" {
    null = true
    type = text
  }
  column "finish" {
    null = true
    type = text
  }
  column "finish_rank" {
    null = true
    type = text
  }
  primary_key {
    columns = [column.network_id, column.work_id]
  }
  index "works_conversation" {
    columns = [column.network_id, column.conversation_id]
  }
}
table "notifications" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "notification_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "kind" {
    null = false
    type = text
  }
  column "text" {
    null = false
    type = text
  }
  column "rank" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.notification_id]
  }
}
table "notification_acks" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "notification_id" {
    null = false
    type = text
  }
  column "client_id" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.notification_id, column.client_id]
  }
}
table "questions" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "question_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "rank" {
    null = false
    type = text
  }
  column "author" {
    null = false
    type = text
  }
  column "created_at_ms" {
    null = false
    type = integer
  }
  column "node" {
    null = false
    type = text
  }
  column "session_id" {
    null = false
    type = text
  }
  column "request_id" {
    null = true
    type = text
  }
  column "body" {
    null = false
    type = text
  }
  column "closed" {
    null = true
    type = text
  }
  primary_key {
    columns = [column.network_id, column.question_id]
  }
  index "questions_session" {
    columns = [column.network_id, column.node, column.session_id]
  }
  index "questions_conversation" {
    columns = [column.network_id, column.conversation_id, column.rank]
  }
}
table "session_owners" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "node" {
    null = false
    type = text
  }
  column "session_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "rank" {
    null = false
    type = text
  }
  column "attribution" {
    null = true
    type = text
  }
  column "muted" {
    null    = false
    type    = integer
    default = 0
  }
  column "created_at_ms" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.network_id, column.node, column.session_id]
  }
}
table "budget_days" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "day" {
    null = false
    type = text
  }
  column "used" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.network_id, column.day]
  }
}
table "triage_seen" {
  schema = schema.main
  column "event_id" {
    null = false
    type = text
  }
  column "ts_ms" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.event_id]
  }
}
table "triage_queue" {
  schema = schema.main
  column "ts_ms" {
    null = false
    type = integer
  }
  column "event_id" {
    null = false
    type = text
  }
  column "request_id" {
    null = true
    type = text
  }
  column "event" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.ts_ms, column.event_id]
  }
}
table "triage_handled" {
  schema = schema.main
  column "event_id" {
    null = false
    type = text
  }
  column "ts_ms" {
    null = false
    type = integer
  }
  column "result" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.event_id]
  }
}
table "triage_outbox" {
  schema = schema.main
  column "event_id" {
    null = false
    type = text
  }
  column "body" {
    null = false
    type = text
  }
  column "state" {
    null = false
    type = text
  }
  column "attempts" {
    null = false
    type = integer
  }
  column "next_ms" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.event_id]
  }
}
table "triage_scans" {
  schema = schema.main
  column "rollout" {
    null = false
    type = text
  }
  column "scanned_to" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.rollout]
  }
}
table "coordination_threads" {
  schema = schema.main
  column "thread_id" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.thread_id]
  }
}
table "addresses" {
  schema = schema.main
  column "device_id" {
    null = false
    type = text
  }
  column "record" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.device_id]
  }
}
table "blob_offsets" {
  schema = schema.main
  column "hash" {
    null = false
    type = text
  }
  column "committed" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.hash]
  }
}
table "identity_meta" {
  schema = schema.main
  column "key" {
    null = true
    type = text
  }
  column "value" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.key]
  }
}
table "invites" {
  schema = schema.main
  column "join_id" {
    null = false
    type = text
  }
  column "invite_hash" {
    null = false
    type = text
  }
  column "epoch" {
    null = false
    type = integer
  }
  column "expires_at_ms" {
    null = false
    type = integer
  }
  column "record" {
    null = false
    type = text
  }
  column "device_id" {
    null = true
    type = text
  }
  column "kind" {
    null = true
    type = text
  }
  column "name" {
    null = true
    type = text
  }
  column "commit_hash" {
    null = true
    type = text
  }
  column "status" {
    null = true
    type = text
  }
  column "admin_id" {
    null = true
    type = text
  }
  column "admin_nonce" {
    null = true
    type = text
  }
  column "joiner_nonce" {
    null = true
    type = text
  }
  column "reason" {
    null = true
    type = text
  }
  column "grant_hash" {
    null = true
    type = text
  }
  primary_key {
    columns = [column.join_id]
  }
  index "invites_invite_hash" {
    unique  = true
    columns = [column.invite_hash]
  }
}
table "scheduler_sessions" {
  schema = schema.main
  column "conversation_id" {
    null = false
    type = text
  }
  column "next_sequence" {
    null = false
    type = integer
  }
  column "active" {
    null = true
    type = text
  }
  column "native_id" {
    null = true
    type = text
  }
  column "native_token" {
    null = true
    type = text
  }
  column "covers" {
    null = false
    type = text
  }
  column "owner_epoch" {
    null = false
    type = integer
  }
  column "sdk_config" {
    null = true
    type = text
  }
  column "permission_version" {
    null = false
    type = integer
  }
  column "shown" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.conversation_id]
  }
}
table "scheduler_jobs" {
  schema = schema.main
  column "conversation_id" {
    null = false
    type = text
  }
  column "trigger" {
    null = false
    type = text
  }
  column "kind" {
    null = false
    type = text
  }
  column "sequence" {
    null = false
    type = integer
  }
  column "basis" {
    null = false
    type = text
  }
  column "reply_to" {
    null = false
    type = text
  }
  column "message_id" {
    null = false
    type = text
  }
  column "state" {
    null = false
    type = text
  }
  column "attempt_id" {
    null = true
    type = text
  }
  column "attempt_boot" {
    null = true
    type = text
  }
  column "attempt_permission" {
    null = true
    type = integer
  }
  column "detail" {
    null = true
    type = text
  }
  column "failure" {
    null = true
    type = text
  }
  primary_key {
    columns = [column.conversation_id, column.trigger]
  }
  index "scheduler_jobs_state" {
    columns = [column.state, column.conversation_id]
  }
}
table "scheduler_outbox" {
  schema = schema.main
  column "conversation_id" {
    null = false
    type = text
  }
  column "trigger" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.conversation_id, column.trigger]
  }
}
table "schedule_claims" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "schedule_id" {
    null = false
    type = text
  }
  column "slot_ms" {
    null = false
    type = integer
  }
  column "owner_epoch" {
    null = false
    type = integer
  }
  column "author" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.conversation_id, column.schedule_id, column.slot_ms]
  }
}
table "pending_checks" {
  schema = schema.main
  column "attempt_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "trigger" {
    null = false
    type = text
  }
  column "detail" {
    null = false
    type = text
  }
  column "created_at_ms" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.attempt_id]
  }
}
table "manager_processes" {
  schema = schema.main
  column "attempt_id" {
    null = false
    type = text
  }
  column "process_group" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.attempt_id, column.process_group]
  }
}
table "manager_turns" {
  schema = schema.main
  column "attempt_id" {
    null = false
    type = text
  }
  column "server" {
    null = false
    type = text
  }
  column "thread_id" {
    null = false
    type = text
  }
  column "turn_id" {
    null = true
    type = text
  }
  primary_key {
    columns = [column.attempt_id]
  }
}
table "work_runs" {
  schema = schema.main
  column "work_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "kind" {
    null = false
    type = text
  }
  column "state" {
    null = false
    type = text
  }
  column "thread_id" {
    null = true
    type = text
  }
  column "turn_id" {
    null = true
    type = text
  }
  column "codex_epoch" {
    null = true
    type = integer
  }
  column "outcome" {
    null = true
    type = text
  }
  column "evidence" {
    null = true
    type = text
  }
  column "probe" {
    null = true
    type = integer
  }
  column "result_text" {
    null = false
    type = text
  }
  column "send_by_ms" {
    null = false
    type = integer
  }
  column "progress" {
    null = false
    type = text
  }
  column "progress_at_ms" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.work_id]
  }
  index "work_runs_thread" {
    columns = [column.thread_id]
  }
}
table "work_dispatches" {
  schema = schema.main
  column "work_id" {
    null = false
    type = text
  }
  column "spec" {
    null = false
    type = text
  }
  column "state" {
    null = false
    type = text
  }
  column "reason" {
    null = true
    type = text
  }
  column "failure" {
    null = true
    type = text
  }
  primary_key {
    columns = [column.work_id]
  }
}
table "work_stops" {
  schema = schema.main
  column "work_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "target_node" {
    null = false
    type = text
  }
  column "requested_at_ms" {
    null = false
    type = integer
  }
  column "deadline_ms" {
    null = false
    type = integer
  }
  column "timed_out" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.work_id]
  }
}
schema "main" {
}
