-- Create "networks" table
CREATE TABLE `networks` (`network_id` text NULL, `bundle` text NOT NULL, PRIMARY KEY (`network_id`));
-- Create "events" table
CREATE TABLE `events` (`network_id` text NOT NULL, `author` text NOT NULL, `seq` integer NOT NULL, `hash` text NOT NULL, `scope_id` text NOT NULL, `lamport` integer NOT NULL, `created_at_ms` integer NOT NULL, `envelope` text NOT NULL, `main` integer NOT NULL, `effective` integer NOT NULL, PRIMARY KEY (`network_id`, `hash`), CONSTRAINT `0` FOREIGN KEY (`network_id`) REFERENCES `networks` (`network_id`) ON UPDATE NO ACTION ON DELETE NO ACTION);
-- Create index "events_main" to table: "events"
CREATE UNIQUE INDEX `events_main` ON `events` (`network_id`, `author`, `seq`) WHERE main = 1;
-- Create index "events_forks" to table: "events"
CREATE INDEX `events_forks` ON `events` (`network_id`, `author`, `seq`) WHERE main = 0;
-- Create index "events_order" to table: "events"
CREATE INDEX `events_order` ON `events` (`network_id`, `scope_id`, `lamport`, `created_at_ms`, `hash`);
-- Create index "events_scope_author" to table: "events"
CREATE INDEX `events_scope_author` ON `events` (`network_id`, `scope_id`, `author`, `seq`);
-- Create "event_blobs" table
CREATE TABLE `event_blobs` (`network_id` text NOT NULL, `hash` text NOT NULL, `blob` text NOT NULL, `size` integer NOT NULL, PRIMARY KEY (`network_id`, `hash`, `blob`));
-- Create index "event_blobs_blob" to table: "event_blobs"
CREATE INDEX `event_blobs_blob` ON `event_blobs` (`network_id`, `blob`);
-- Create "log_heads" table
CREATE TABLE `log_heads` (`network_id` text NOT NULL, `author` text NOT NULL, `max_seq` integer NOT NULL, `head_hash` text NOT NULL, PRIMARY KEY (`network_id`, `author`));
-- Create "pending_events" table
CREATE TABLE `pending_events` (`network_id` text NOT NULL, `hash` text NOT NULL, `author` text NOT NULL, `seq` integer NOT NULL, `envelope` text NOT NULL, `missing` text NOT NULL, PRIMARY KEY (`network_id`, `hash`));
-- Create index "pending_events_missing" to table: "pending_events"
CREATE INDEX `pending_events_missing` ON `pending_events` (`network_id`, `missing`);
-- Create "conversations" table
CREATE TABLE `conversations` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `title` text NULL, `title_rank` text NULL, `owner` text NULL, `owner_epoch` integer NOT NULL DEFAULT 0, `owner_key` text NOT NULL DEFAULT '', PRIMARY KEY (`network_id`, `conversation_id`));
-- Create "messages" table
CREATE TABLE `messages` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `message_id` text NOT NULL, `event_hash` text NOT NULL, `rank` text NOT NULL, `author` text NOT NULL, `role` text NOT NULL, `text` text NOT NULL, `created_at_ms` integer NOT NULL, `in_reply_to` text NULL, `status` text NULL, `report_results` integer NULL, `answers` text NULL, `attachments` text NOT NULL, `sender` text NULL, PRIMARY KEY (`network_id`, `conversation_id`, `message_id`));
-- Create index "messages_order" to table: "messages"
CREATE INDEX `messages_order` ON `messages` (`network_id`, `conversation_id`, `rank`);
-- Create index "messages_answers" to table: "messages"
CREATE INDEX `messages_answers` ON `messages` (`network_id`, `answers`);
-- Create "bots" table
CREATE TABLE `bots` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `bot_id` text NOT NULL, `name` text NOT NULL, `instructions` text NOT NULL, `project` text NULL, `event_hash` text NOT NULL, `rank` text NOT NULL, `cause` text NULL, PRIMARY KEY (`network_id`, `conversation_id`));
-- Create "bot_profiles" table
CREATE TABLE `bot_profiles` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `field` text NOT NULL, `value` text NOT NULL, `rank` text NOT NULL, PRIMARY KEY (`network_id`, `conversation_id`, `field`));
-- Create "bot_deletions" table
CREATE TABLE `bot_deletions` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `event_hash` text NOT NULL, `rank` text NOT NULL, PRIMARY KEY (`network_id`, `conversation_id`));
-- Create "skills" table
CREATE TABLE `skills` (`network_id` text NOT NULL, `skill_id` text NOT NULL, `skill` text NULL, `rank` text NOT NULL, PRIMARY KEY (`network_id`, `skill_id`));
-- Create "schedules" table
CREATE TABLE `schedules` (`network_id` text NOT NULL, `schedule_id` text NOT NULL, `conversation_id` text NOT NULL, `schedule` text NULL, `rank` text NOT NULL, `changed_at_ms` integer NOT NULL, `enabled` integer NOT NULL, `enabled_rank` text NOT NULL, `enabled_at_ms` integer NOT NULL, `removed` integer NOT NULL, PRIMARY KEY (`network_id`, `schedule_id`));
-- Create index "schedules_conversation" to table: "schedules"
CREATE INDEX `schedules_conversation` ON `schedules` (`network_id`, `conversation_id`);
-- Create "memories" table
CREATE TABLE `memories` (`network_id` text NOT NULL, `memory_id` text NOT NULL, `conversation_id` text NOT NULL, `text` text NULL, `rank` text NOT NULL, `changed_at_ms` integer NOT NULL, PRIMARY KEY (`network_id`, `memory_id`));
-- Create index "memories_conversation" to table: "memories"
CREATE INDEX `memories_conversation` ON `memories` (`network_id`, `conversation_id`);
-- Create "bot_changes" table
CREATE TABLE `bot_changes` (`network_id` text NOT NULL, `change_id` text NOT NULL, `conversation_id` text NOT NULL, `event_hash` text NOT NULL, `rank` text NOT NULL, `cause` text NULL, `change` text NOT NULL, `created_at_ms` integer NOT NULL, PRIMARY KEY (`network_id`, `change_id`));
-- Create index "bot_changes_order" to table: "bot_changes"
CREATE INDEX `bot_changes_order` ON `bot_changes` (`network_id`, `conversation_id`, `rank`);
-- Create "works" table
CREATE TABLE `works` (`network_id` text NOT NULL, `work_id` text NOT NULL, `conversation_id` text NOT NULL, `target_node` text NULL, `dispatch` text NULL, `dispatch_rank` text NULL, `progress` text NULL, `progress_rank` text NULL, `thread_id` text NULL, `turn_id` text NULL, `finish` text NULL, `finish_rank` text NULL, PRIMARY KEY (`network_id`, `work_id`));
-- Create index "works_conversation" to table: "works"
CREATE INDEX `works_conversation` ON `works` (`network_id`, `conversation_id`);
-- Create "notifications" table
CREATE TABLE `notifications` (`network_id` text NOT NULL, `notification_id` text NOT NULL, `conversation_id` text NOT NULL, `kind` text NOT NULL, `text` text NOT NULL, `rank` text NOT NULL, PRIMARY KEY (`network_id`, `notification_id`));
-- Create "notification_acks" table
CREATE TABLE `notification_acks` (`network_id` text NOT NULL, `notification_id` text NOT NULL, `client_id` text NOT NULL, PRIMARY KEY (`network_id`, `notification_id`, `client_id`));
-- Create "questions" table
CREATE TABLE `questions` (`network_id` text NOT NULL, `question_id` text NOT NULL, `conversation_id` text NOT NULL, `event_hash` text NOT NULL, `rank` text NOT NULL, `author` text NOT NULL, `created_at_ms` integer NOT NULL, `node` text NOT NULL, `session_id` text NOT NULL, `request_id` text NULL, `body` text NOT NULL, PRIMARY KEY (`network_id`, `question_id`));
-- Create index "questions_session" to table: "questions"
CREATE INDEX `questions_session` ON `questions` (`network_id`, `node`, `session_id`);
-- Create index "questions_conversation" to table: "questions"
CREATE INDEX `questions_conversation` ON `questions` (`network_id`, `conversation_id`, `rank`);
-- Create "session_owners" table
CREATE TABLE `session_owners` (`network_id` text NOT NULL, `node` text NOT NULL, `session_id` text NOT NULL, `conversation_id` text NOT NULL, `rank` text NOT NULL, `attribution` text NULL, `created_at_ms` integer NOT NULL, PRIMARY KEY (`network_id`, `node`, `session_id`));
-- Create "budget_days" table
CREATE TABLE `budget_days` (`network_id` text NOT NULL, `day` text NOT NULL, `used` integer NOT NULL, PRIMARY KEY (`network_id`, `day`));
-- Create "triage_seen" table
CREATE TABLE `triage_seen` (`event_id` text NOT NULL, `ts_ms` integer NOT NULL, PRIMARY KEY (`event_id`));
-- Create "triage_queue" table
CREATE TABLE `triage_queue` (`ts_ms` integer NOT NULL, `event_id` text NOT NULL, `request_id` text NULL, `event` text NOT NULL, PRIMARY KEY (`ts_ms`, `event_id`));
-- Create "triage_handled" table
CREATE TABLE `triage_handled` (`event_id` text NOT NULL, `ts_ms` integer NOT NULL, `result` text NOT NULL, PRIMARY KEY (`event_id`));
-- Create "triage_outbox" table
CREATE TABLE `triage_outbox` (`event_id` text NOT NULL, `body` text NOT NULL, `state` text NOT NULL, `attempts` integer NOT NULL, `next_ms` integer NOT NULL, `created_ms` integer NOT NULL, `last_error` text NULL, PRIMARY KEY (`event_id`));
-- Create "triage_scans" table
CREATE TABLE `triage_scans` (`rollout` text NOT NULL, `scanned_to` integer NOT NULL, PRIMARY KEY (`rollout`));
-- Create "addresses" table
CREATE TABLE `addresses` (`device_id` text NOT NULL, `record` text NOT NULL, PRIMARY KEY (`device_id`));
-- Create "blob_offsets" table
CREATE TABLE `blob_offsets` (`hash` text NOT NULL, `committed` integer NOT NULL, PRIMARY KEY (`hash`));
-- Create "identity_meta" table
CREATE TABLE `identity_meta` (`key` text NULL, `value` text NOT NULL, PRIMARY KEY (`key`));
-- Create "invites" table
CREATE TABLE `invites` (`join_id` text NOT NULL, `invite_hash` text NOT NULL, `epoch` integer NOT NULL, `expires_at_ms` integer NOT NULL, `record` text NOT NULL, `device_id` text NULL, `kind` text NULL, `name` text NULL, `commit_hash` text NULL, `status` text NULL, `admin_id` text NULL, `admin_nonce` text NULL, `joiner_nonce` text NULL, `reason` text NULL, `grant_hash` text NULL, PRIMARY KEY (`join_id`));
-- Create index "invites_invite_hash" to table: "invites"
CREATE UNIQUE INDEX `invites_invite_hash` ON `invites` (`invite_hash`);
-- Create "scheduler_sessions" table
CREATE TABLE `scheduler_sessions` (`conversation_id` text NOT NULL, `next_sequence` integer NOT NULL, `active` text NULL, `native_id` text NULL, `native_token` text NULL, `covers` text NOT NULL, `owner_epoch` integer NOT NULL, `sdk_config` text NULL, `permission_version` integer NOT NULL, PRIMARY KEY (`conversation_id`));
-- Create "scheduler_jobs" table
CREATE TABLE `scheduler_jobs` (`conversation_id` text NOT NULL, `trigger` text NOT NULL, `kind` text NOT NULL, `sequence` integer NOT NULL, `basis` text NOT NULL, `reply_to` text NOT NULL, `message_id` text NOT NULL, `state` text NOT NULL, `attempt_id` text NULL, `attempt_boot` text NULL, `attempt_permission` integer NULL, `detail` text NULL, PRIMARY KEY (`conversation_id`, `trigger`));
-- Create index "scheduler_jobs_state" to table: "scheduler_jobs"
CREATE INDEX `scheduler_jobs_state` ON `scheduler_jobs` (`state`, `conversation_id`);
-- Create "scheduler_outbox" table
CREATE TABLE `scheduler_outbox` (`conversation_id` text NOT NULL, `trigger` text NOT NULL, PRIMARY KEY (`conversation_id`, `trigger`));
-- Create "schedule_claims" table
CREATE TABLE `schedule_claims` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `schedule_id` text NOT NULL, `slot_ms` integer NOT NULL, `owner_epoch` integer NOT NULL, `author` text NOT NULL, PRIMARY KEY (`network_id`, `conversation_id`, `schedule_id`, `slot_ms`));
-- Create "pending_checks" table
CREATE TABLE `pending_checks` (`attempt_id` text NOT NULL, `conversation_id` text NOT NULL, `trigger` text NOT NULL, `detail` text NOT NULL, `created_at_ms` integer NOT NULL, PRIMARY KEY (`attempt_id`));
-- Create "manager_processes" table
CREATE TABLE `manager_processes` (`attempt_id` text NOT NULL, `process_group` integer NOT NULL, PRIMARY KEY (`attempt_id`, `process_group`));
-- Create "manager_turns" table
CREATE TABLE `manager_turns` (`attempt_id` text NOT NULL, `thread_id` text NOT NULL, `turn_id` text NULL, PRIMARY KEY (`attempt_id`));
-- Create "work_runs" table
CREATE TABLE `work_runs` (`work_id` text NOT NULL, `conversation_id` text NOT NULL, `kind` text NOT NULL, `state` text NOT NULL, `thread_id` text NULL, `turn_id` text NULL, `codex_epoch` integer NULL, `outcome` text NULL, `evidence` text NULL, `probe` integer NULL, `result_text` text NOT NULL, `send_by_ms` integer NOT NULL, `progress` text NOT NULL, `progress_at_ms` integer NOT NULL, PRIMARY KEY (`work_id`));
-- Create "work_dispatches" table
CREATE TABLE `work_dispatches` (`work_id` text NOT NULL, `conversation_id` text NOT NULL, `target_node` text NOT NULL, `spec` text NOT NULL, `state` text NOT NULL, `reason` text NULL, PRIMARY KEY (`work_id`));
-- Create "work_stops" table
CREATE TABLE `work_stops` (`work_id` text NOT NULL, `conversation_id` text NOT NULL, `target_node` text NOT NULL, `requested_at_ms` integer NOT NULL, `deadline_ms` integer NOT NULL, `timed_out` integer NOT NULL, PRIMARY KEY (`work_id`));

PRAGMA user_version = 1;
