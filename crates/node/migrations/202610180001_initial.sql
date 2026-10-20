-- Create "networks" table
CREATE TABLE `networks` (`network_id` text NULL, `revision` integer NOT NULL, `bundle` text NOT NULL, PRIMARY KEY (`network_id`));
-- Create "events" table
CREATE TABLE `events` (`network_id` text NOT NULL, `author` text NOT NULL, `seq` integer NOT NULL, `hash` text NOT NULL, `scope_id` text NOT NULL, `lamport` integer NOT NULL, `created_at_ms` integer NOT NULL, `envelope` text NOT NULL, PRIMARY KEY (`network_id`, `author`, `seq`), CONSTRAINT `0` FOREIGN KEY (`network_id`) REFERENCES `networks` (`network_id`) ON UPDATE NO ACTION ON DELETE NO ACTION);
-- Create index "events_hash" to table: "events"
CREATE UNIQUE INDEX `events_hash` ON `events` (`network_id`, `hash`);
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
CREATE TABLE `pending_events` (`network_id` text NOT NULL, `hash` text NOT NULL, `author` text NOT NULL, `seq` integer NOT NULL, `envelope` text NOT NULL, `missing` text NOT NULL, `received_at_ms` integer NOT NULL, PRIMARY KEY (`network_id`, `hash`));
-- Create index "pending_events_missing" to table: "pending_events"
CREATE INDEX `pending_events_missing` ON `pending_events` (`network_id`, `missing`);
-- Create "log_conflicts" table
CREATE TABLE `log_conflicts` (`network_id` text NOT NULL, `hash` text NOT NULL, `author` text NOT NULL, `seq` integer NOT NULL, `envelope` text NOT NULL, `kept_hash` text NOT NULL, `detected_at_ms` integer NOT NULL, PRIMARY KEY (`network_id`, `hash`));
-- Create index "log_conflicts_author" to table: "log_conflicts"
CREATE INDEX `log_conflicts_author` ON `log_conflicts` (`network_id`, `author`, `seq`);
-- Create "conversations" table
CREATE TABLE `conversations` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `title` text NULL, `title_rank` text NULL, `owner` text NULL, `owner_epoch` integer NOT NULL DEFAULT 0, `owner_key` text NOT NULL DEFAULT '', PRIMARY KEY (`network_id`, `conversation_id`));
-- Create "messages" table
CREATE TABLE `messages` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `message_id` text NOT NULL, `event_hash` text NOT NULL, `rank` text NOT NULL, `author` text NOT NULL, `role` text NOT NULL, `text` text NOT NULL, `created_at_ms` integer NOT NULL, `in_reply_to` text NULL, `status` text NULL, `report_results` integer NULL, `blobs` text NOT NULL, PRIMARY KEY (`network_id`, `conversation_id`, `message_id`));
-- Create index "messages_order" to table: "messages"
CREATE INDEX `messages_order` ON `messages` (`network_id`, `conversation_id`, `rank`);
-- Create "bots" table
CREATE TABLE `bots` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `bot_id` text NOT NULL, `name` text NOT NULL, `instructions` text NOT NULL, `project` text NULL, `event_hash` text NOT NULL, `rank` text NOT NULL, PRIMARY KEY (`network_id`, `conversation_id`));
-- Create "assistant_profiles" table
CREATE TABLE `assistant_profiles` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `field` text NOT NULL, `value` text NOT NULL, `rank` text NOT NULL, PRIMARY KEY (`network_id`, `conversation_id`, `field`));
-- Create "skills" table
CREATE TABLE `skills` (`network_id` text NOT NULL, `skill_id` text NOT NULL, `skill` text NULL, `rank` text NOT NULL, PRIMARY KEY (`network_id`, `skill_id`));
-- Create "routines" table
CREATE TABLE `routines` (`network_id` text NOT NULL, `routine_id` text NOT NULL, `conversation_id` text NOT NULL, `routine` text NULL, `rank` text NOT NULL, `changed_at_ms` integer NOT NULL, PRIMARY KEY (`network_id`, `routine_id`));
-- Create index "routines_conversation" to table: "routines"
CREATE INDEX `routines_conversation` ON `routines` (`network_id`, `conversation_id`);
-- Create "assistant_changes" table
CREATE TABLE `assistant_changes` (`network_id` text NOT NULL, `change_id` text NOT NULL, `conversation_id` text NOT NULL, `event_hash` text NOT NULL, `rank` text NOT NULL, `cause` text NOT NULL, `change` text NOT NULL, `created_at_ms` integer NOT NULL, PRIMARY KEY (`network_id`, `change_id`));
-- Create index "assistant_changes_order" to table: "assistant_changes"
CREATE INDEX `assistant_changes_order` ON `assistant_changes` (`network_id`, `conversation_id`, `rank`);
-- Create "works" table
CREATE TABLE `works` (`network_id` text NOT NULL, `work_id` text NOT NULL, `conversation_id` text NOT NULL, `target_node` text NULL, `dispatch` text NULL, `dispatch_rank` text NULL, `progress` text NULL, `progress_rank` text NULL, `thread_id` text NULL, `turn_id` text NULL, `finish` text NULL, `finish_rank` text NULL, PRIMARY KEY (`network_id`, `work_id`));
-- Create index "works_conversation" to table: "works"
CREATE INDEX `works_conversation` ON `works` (`network_id`, `conversation_id`);
-- Create "approvals" table
CREATE TABLE `approvals` (`network_id` text NOT NULL, `request_id` text NOT NULL, `work_id` text NOT NULL, `kind` text NULL, `summary` text NULL, `request_rank` text NULL, `decision` text NULL, `answer_key` text NULL, PRIMARY KEY (`network_id`, `request_id`));
-- Create index "approvals_work" to table: "approvals"
CREATE INDEX `approvals_work` ON `approvals` (`network_id`, `work_id`);
-- Create "autonomy" table
CREATE TABLE `autonomy` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `mode` text NOT NULL, `strictness` integer NOT NULL, `version` integer NOT NULL, `request_key` text NOT NULL, PRIMARY KEY (`network_id`, `conversation_id`));
-- Create "notifications" table
CREATE TABLE `notifications` (`network_id` text NOT NULL, `notification_id` text NOT NULL, `conversation_id` text NOT NULL, `kind` text NOT NULL, `text` text NOT NULL, `rank` text NOT NULL, PRIMARY KEY (`network_id`, `notification_id`));
-- Create "notification_acks" table
CREATE TABLE `notification_acks` (`network_id` text NOT NULL, `notification_id` text NOT NULL, `client_id` text NOT NULL, PRIMARY KEY (`network_id`, `notification_id`, `client_id`));
-- Create "budget_days" table
CREATE TABLE `budget_days` (`network_id` text NOT NULL, `day` text NOT NULL, `used` integer NOT NULL, PRIMARY KEY (`network_id`, `day`));
-- Create "local_values" table
CREATE TABLE `local_values` (`network_id` text NOT NULL, `key` text NOT NULL, `value` text NOT NULL, PRIMARY KEY (`network_id`, `key`));
-- Create "identity_meta" table
CREATE TABLE `identity_meta` (`key` text NULL, `value` text NOT NULL, PRIMARY KEY (`key`));

-- Create "invites" table
CREATE TABLE `invites` (`join_id` text NOT NULL, `invite_hash` text NOT NULL, `issuer` text NOT NULL, `epoch` integer NOT NULL, `expires_at_ms` integer NOT NULL, `record` text NOT NULL, `device_id` text NULL, `kind` text NULL, `name` text NULL, `commit_hash` text NULL, `status` text NULL, `admin_id` text NULL, `admin_nonce` text NULL, `joiner_nonce` text NULL, `reason` text NULL, `grant_hash` text NULL, PRIMARY KEY (`join_id`));
-- Create index "invites_invite_hash" to table: "invites"
CREATE UNIQUE INDEX `invites_invite_hash` ON `invites` (`invite_hash`);

PRAGMA user_version = 7;
