-- Create "schema_metadata" table
CREATE TABLE `schema_metadata` (`singleton` integer NOT NULL, `format_id` text NOT NULL, `data_epoch` integer NOT NULL, `layout_version` integer NOT NULL, `schema_fingerprint` text NOT NULL, `initialization_state` text NOT NULL, PRIMARY KEY (`singleton`), CONSTRAINT `metadata_singleton` CHECK (singleton = 1), CONSTRAINT `metadata_versions` CHECK (data_epoch > 0 AND layout_version > 0), CONSTRAINT `metadata_initialized` CHECK (initialization_state = 'ready'));
-- Create "spaces" table
CREATE TABLE `spaces` (`space_id` text NOT NULL, `revision` integer NOT NULL, `bundle` text NOT NULL, PRIMARY KEY (`space_id`), CONSTRAINT `space_revision` CHECK (revision > 0));
-- Create "node_certificates" table
CREATE TABLE `node_certificates` (`space_id` text NOT NULL, `node_id` text NOT NULL, `public_key` text NOT NULL, PRIMARY KEY (`space_id`, `node_id`), CONSTRAINT `node_certificates_space` FOREIGN KEY (`space_id`) REFERENCES `spaces` (`space_id`));
-- Create "local_devices" table
CREATE TABLE `local_devices` (`space_id` text NOT NULL, `device_id` text NOT NULL, `public_key` text NOT NULL, PRIMARY KEY (`space_id`, `device_id`), CONSTRAINT `local_devices_certificate` FOREIGN KEY (`space_id`, `device_id`) REFERENCES `node_certificates` (`space_id`, `node_id`));
-- Create "events" table
CREATE TABLE `events` (`space_id` text NOT NULL, `hash` text NOT NULL, `scope_id` text NOT NULL, `author` text NOT NULL, `sequence` integer NOT NULL, `envelope` text NOT NULL, `status` text NOT NULL, PRIMARY KEY (`space_id`, `hash`), CONSTRAINT `events_author` FOREIGN KEY (`space_id`, `author`) REFERENCES `node_certificates` (`space_id`, `node_id`), CONSTRAINT `event_sequence` CHECK (sequence > 0), CONSTRAINT `event_status` CHECK (status IN ('pending', 'ready', 'conflict', 'invalid')));
-- Create index "events_author_sequence" to table: "events"
CREATE INDEX `events_author_sequence` ON `events` (`space_id`, `scope_id`, `author`, `sequence`);
-- Create index "events_scope" to table: "events"
CREATE INDEX `events_scope` ON `events` (`space_id`, `scope_id`);
-- Create "blobs" table
CREATE TABLE `blobs` (`space_id` text NOT NULL, `hash` text NOT NULL, `size` integer NOT NULL, `content` blob NOT NULL, PRIMARY KEY (`space_id`, `hash`), CONSTRAINT `blobs_space` FOREIGN KEY (`space_id`) REFERENCES `spaces` (`space_id`), CONSTRAINT `blob_nonnegative_size` CHECK (size >= 0));
-- Create "conversations" table
CREATE TABLE `conversations` (`space_id` text NOT NULL, `conversation_id` text NOT NULL, `title` text NOT NULL, PRIMARY KEY (`space_id`, `conversation_id`), CONSTRAINT `conversations_space` FOREIGN KEY (`space_id`) REFERENCES `spaces` (`space_id`));
-- Create "messages" table
CREATE TABLE `messages` (`space_id` text NOT NULL, `conversation_id` text NOT NULL, `event_hash` text NOT NULL, `message_id` text NOT NULL, `role` text NOT NULL, `text` text NOT NULL, `created_at_ms` integer NOT NULL, `ordinal` integer NOT NULL, PRIMARY KEY (`space_id`, `event_hash`), CONSTRAINT `messages_source` FOREIGN KEY (`space_id`, `event_hash`) REFERENCES `events` (`space_id`, `hash`));
-- Create "tasks" table
CREATE TABLE `tasks` (`space_id` text NOT NULL, `conversation_id` text NOT NULL, `task_id` text NOT NULL, `title` text NOT NULL, `goal` text NOT NULL, `acceptance` text NOT NULL, `status` text NOT NULL, `detail` text NOT NULL, `event_hash` text NOT NULL, PRIMARY KEY (`space_id`, `conversation_id`, `task_id`), CONSTRAINT `tasks_source` FOREIGN KEY (`space_id`, `event_hash`) REFERENCES `events` (`space_id`, `hash`));
-- Create "execution_sessions" table
CREATE TABLE `execution_sessions` (`space_id` text NOT NULL, `target_node` text NOT NULL, `session_id` text NOT NULL, `revision` integer NOT NULL, `record` text NOT NULL, PRIMARY KEY (`space_id`, `target_node`, `session_id`), CONSTRAINT `sessions_node` FOREIGN KEY (`space_id`, `target_node`) REFERENCES `node_certificates` (`space_id`, `node_id`), CONSTRAINT `session_revision` CHECK (revision >= 0));
-- Create "operations" table
CREATE TABLE `operations` (`space_id` text NOT NULL, `operation_id` text NOT NULL, `target_node` text NOT NULL, `session_id` text NOT NULL, `request` text NOT NULL, `state` text NOT NULL, `control` integer NOT NULL DEFAULT 0, `result` text NOT NULL, PRIMARY KEY (`space_id`, `operation_id`), CONSTRAINT `operations_node` FOREIGN KEY (`space_id`, `target_node`) REFERENCES `node_certificates` (`space_id`, `node_id`), CONSTRAINT `operation_control` CHECK (control IN (0,1)));
-- Create index "operations_session" to table: "operations"
CREATE INDEX `operations_session` ON `operations` (`space_id`, `target_node`, `session_id`, `state`);
-- Create index "operations_request_reference" to table: "operations"
CREATE UNIQUE INDEX `operations_request_reference` ON `operations` (`space_id`, `operation_id`, `request`);
-- Create "resource_reservations" table
CREATE TABLE `resource_reservations` (`space_id` text NOT NULL, `operation_id` text NOT NULL, `target_node` text NOT NULL, `resource` text NOT NULL, PRIMARY KEY (`space_id`, `operation_id`, `resource`), CONSTRAINT `reservations_operation` FOREIGN KEY (`space_id`, `operation_id`) REFERENCES `operations` (`space_id`, `operation_id`));
-- Create "continuation_claims" table
CREATE TABLE `continuation_claims` (`space_id` text NOT NULL, `result_event_hash` text NOT NULL, `policy_version` text NOT NULL, `record` text NOT NULL, PRIMARY KEY (`space_id`, `result_event_hash`, `policy_version`), CONSTRAINT `continuations_result` FOREIGN KEY (`space_id`, `result_event_hash`) REFERENCES `events` (`space_id`, `hash`));
-- Create "notification_outbox" table
CREATE TABLE `notification_outbox` (`space_id` text NOT NULL, `notification_id` text NOT NULL, `record` text NOT NULL, `derived` integer NOT NULL DEFAULT 0, PRIMARY KEY (`space_id`, `notification_id`), CONSTRAINT `notification_outbox_space` FOREIGN KEY (`space_id`) REFERENCES `spaces` (`space_id`), CONSTRAINT `notification_derived` CHECK (derived IN (0,1)));
-- Create "notification_acks" table
CREATE TABLE `notification_acks` (`space_id` text NOT NULL, `notification_id` text NOT NULL, `client_id` text NOT NULL, `event_hash` text NOT NULL DEFAULT '', PRIMARY KEY (`space_id`, `notification_id`, `client_id`), CONSTRAINT `notification_acks_space` FOREIGN KEY (`space_id`) REFERENCES `spaces` (`space_id`));
-- Create "budget_days" table
CREATE TABLE `budget_days` (`space_id` text NOT NULL, `day` text NOT NULL, `used` integer NOT NULL, PRIMARY KEY (`space_id`, `day`), CONSTRAINT `budget_days_space` FOREIGN KEY (`space_id`) REFERENCES `spaces` (`space_id`), CONSTRAINT `budget_nonnegative` CHECK (used >= 0));
-- Create "local_values" table
CREATE TABLE `local_values` (`space_id` text NOT NULL, `key` text NOT NULL, `value` text NOT NULL, PRIMARY KEY (`space_id`, `key`), CONSTRAINT `local_values_space` FOREIGN KEY (`space_id`) REFERENCES `spaces` (`space_id`));
-- Create "identity_meta" table
CREATE TABLE `identity_meta` (`key` text NOT NULL, `value` text NOT NULL, PRIMARY KEY (`key`));
-- Create "identity_requests" table
CREATE TABLE `identity_requests` (`space_id` text NOT NULL, `request_id` text NOT NULL, `subject_id` text NOT NULL, `kind` text NOT NULL, `response` text NOT NULL, `created_at_ms` integer NOT NULL, PRIMARY KEY (`space_id`, `request_id`), CONSTRAINT `identity_requests_space` FOREIGN KEY (`space_id`) REFERENCES `spaces` (`space_id`));
-- Create "identity_invites" table
CREATE TABLE `identity_invites` (`space_id` text NOT NULL, `invite_id` text NOT NULL, `request_id` text NOT NULL, `inviter_subject_id` text NOT NULL, `payload` text NOT NULL, `payload_hash` text NOT NULL, `capability_hash` text NOT NULL, `inviter_signature` text NOT NULL, `epoch` integer NOT NULL, `expires_at_ms` integer NOT NULL, `used` integer NOT NULL DEFAULT 0, PRIMARY KEY (`space_id`, `invite_id`), CONSTRAINT `identity_invites_space` FOREIGN KEY (`space_id`) REFERENCES `spaces` (`space_id`), CONSTRAINT `identity_invites_used` CHECK (used IN (0,1)), CONSTRAINT `identity_invites_epoch` CHECK (epoch > 0));
-- Create index "identity_invites_request" to table: "identity_invites"
CREATE UNIQUE INDEX `identity_invites_request` ON `identity_invites` (`space_id`, `request_id`);
-- Create "identity_pair_requests" table
CREATE TABLE `identity_pair_requests` (`space_id` text NOT NULL, `request_id` text NOT NULL, `invite_id` text NOT NULL, `subject_id` text NOT NULL, `review` text NOT NULL, `review_hash` text NOT NULL, `new_subject_signature` text NOT NULL DEFAULT '', PRIMARY KEY (`space_id`, `request_id`), CONSTRAINT `pair_requests_invite` FOREIGN KEY (`space_id`, `invite_id`) REFERENCES `identity_invites` (`space_id`, `invite_id`));
-- Create index "pair_requests_invite_once" to table: "identity_pair_requests"
CREATE UNIQUE INDEX `pair_requests_invite_once` ON `identity_pair_requests` (`space_id`, `invite_id`);
-- Create "identity_pair_reviews" table
CREATE TABLE `identity_pair_reviews` (`space_id` text NOT NULL, `invite_id` text NOT NULL, `review` text NOT NULL, `review_hash` text NOT NULL, `inviter_signature` text NOT NULL, `new_subject_signature` text NOT NULL, `created_at_ms` integer NOT NULL, PRIMARY KEY (`space_id`, `invite_id`), CONSTRAINT `pair_reviews_request` FOREIGN KEY (`space_id`, `invite_id`) REFERENCES `identity_pair_requests` (`space_id`, `invite_id`));
-- Create "accounts" table
CREATE TABLE `accounts` (`account_id` text NOT NULL, `root_public_key` text NOT NULL, `public_record` text NOT NULL, PRIMARY KEY (`account_id`));
-- Create "devices" table
CREATE TABLE `devices` (`space_id` text NOT NULL, `device_id` text NOT NULL, `account_id` text NOT NULL, `kind` text NOT NULL, `public_key` text NOT NULL, `certificate` text NOT NULL, PRIMARY KEY (`space_id`, `device_id`), CONSTRAINT `devices_space` FOREIGN KEY (`space_id`) REFERENCES `spaces` (`space_id`), CONSTRAINT `devices_account` FOREIGN KEY (`account_id`) REFERENCES `accounts` (`account_id`), CONSTRAINT `device_kind` CHECK (kind IN ('node', 'client')));
-- Create "members" table
CREATE TABLE `members` (`space_id` text NOT NULL, `account_id` text NOT NULL, `role` text NOT NULL, `signed_record` text NOT NULL, PRIMARY KEY (`space_id`, `account_id`), CONSTRAINT `members_space` FOREIGN KEY (`space_id`) REFERENCES `spaces` (`space_id`), CONSTRAINT `members_account` FOREIGN KEY (`account_id`) REFERENCES `accounts` (`account_id`), CONSTRAINT `member_role` CHECK (role IN ('owner', 'admin', 'member')));
-- Create "resources" table
CREATE TABLE `resources` (`space_id` text NOT NULL, `resource_id` text NOT NULL, `owner_account_id` text NOT NULL, `host_node_id` text NOT NULL, `kind` text NOT NULL, PRIMARY KEY (`space_id`, `resource_id`), CONSTRAINT `resources_owner` FOREIGN KEY (`owner_account_id`) REFERENCES `accounts` (`account_id`), CONSTRAINT `resources_host` FOREIGN KEY (`space_id`, `host_node_id`) REFERENCES `devices` (`space_id`, `device_id`), CONSTRAINT `resource_kind` CHECK (kind IN ('agent', 'relay', 'storage')));
-- Create "grants" table
CREATE TABLE `grants` (`space_id` text NOT NULL, `grant_id` text NOT NULL, `resource_id` text NOT NULL, `grantee_account_id` text NOT NULL, `parent_grant_id` text NULL, `action` text NOT NULL, `signed_envelope` text NOT NULL, PRIMARY KEY (`space_id`, `grant_id`), CONSTRAINT `grants_resource` FOREIGN KEY (`space_id`, `resource_id`) REFERENCES `resources` (`space_id`, `resource_id`), CONSTRAINT `grants_grantee` FOREIGN KEY (`grantee_account_id`) REFERENCES `accounts` (`account_id`), CONSTRAINT `grants_parent` FOREIGN KEY (`space_id`, `parent_grant_id`) REFERENCES `grants` (`space_id`, `grant_id`));
-- Create "policy_events" table
CREATE TABLE `policy_events` (`space_id` text NOT NULL, `hash` text NOT NULL, `authority_epoch` integer NOT NULL, `sequence` integer NOT NULL, `signed_envelope` text NOT NULL, PRIMARY KEY (`space_id`, `hash`), CONSTRAINT `policy_events_space` FOREIGN KEY (`space_id`) REFERENCES `spaces` (`space_id`), CONSTRAINT `policy_monotonic_values` CHECK (authority_epoch > 0 AND sequence > 0));
-- Create index "policy_accepted_sequence" to table: "policy_events"
CREATE UNIQUE INDEX `policy_accepted_sequence` ON `policy_events` (`space_id`, `authority_epoch`, `sequence`);
-- Create index "policy_head_reference" to table: "policy_events"
CREATE UNIQUE INDEX `policy_head_reference` ON `policy_events` (`space_id`, `authority_epoch`, `sequence`, `hash`);
-- Create "policy_heads" table
CREATE TABLE `policy_heads` (`space_id` text NOT NULL, `authority_epoch` integer NOT NULL, `sequence` integer NOT NULL, `head_hash` text NOT NULL, PRIMARY KEY (`space_id`), CONSTRAINT `policy_heads_event` FOREIGN KEY (`space_id`, `authority_epoch`, `sequence`, `head_hash`) REFERENCES `policy_events` (`space_id`, `authority_epoch`, `sequence`, `hash`));
-- Create "authorization_decisions" table
CREATE TABLE `authorization_decisions` (`space_id` text NOT NULL, `operation_id` text NOT NULL, `policy_hash` text NOT NULL, `actor_account_id` text NOT NULL, `receipt` text NOT NULL, PRIMARY KEY (`space_id`, `operation_id`), CONSTRAINT `decisions_operation` FOREIGN KEY (`space_id`, `operation_id`) REFERENCES `operations` (`space_id`, `operation_id`), CONSTRAINT `decisions_policy` FOREIGN KEY (`space_id`, `policy_hash`) REFERENCES `policy_events` (`space_id`, `hash`), CONSTRAINT `decisions_actor` FOREIGN KEY (`actor_account_id`) REFERENCES `accounts` (`account_id`));
-- Create "event_dependencies" table
CREATE TABLE `event_dependencies` (`space_id` text NOT NULL, `child_hash` text NOT NULL, `parent_hash` text NOT NULL, PRIMARY KEY (`space_id`, `child_hash`, `parent_hash`), CONSTRAINT `dependencies_child` FOREIGN KEY (`space_id`, `child_hash`) REFERENCES `events` (`space_id`, `hash`));
-- Create "event_blobs" table
CREATE TABLE `event_blobs` (`space_id` text NOT NULL, `event_hash` text NOT NULL, `blob_hash` text NOT NULL, PRIMARY KEY (`space_id`, `event_hash`, `blob_hash`), CONSTRAINT `event_blobs_event` FOREIGN KEY (`space_id`, `event_hash`) REFERENCES `events` (`space_id`, `hash`));
-- Create "resource_claims" table
CREATE TABLE `resource_claims` (`space_id` text NOT NULL, `resource_id` text NOT NULL, `operation_id` text NOT NULL, PRIMARY KEY (`space_id`, `resource_id`), CONSTRAINT `claims_resource` FOREIGN KEY (`space_id`, `resource_id`) REFERENCES `resources` (`space_id`, `resource_id`), CONSTRAINT `claims_operation` FOREIGN KEY (`space_id`, `operation_id`) REFERENCES `operations` (`space_id`, `operation_id`));
-- Create "nonce_receipts" table
CREATE TABLE `nonce_receipts` (`space_id` text NOT NULL, `nonce` text NOT NULL, `body_hash` text NOT NULL, `request_body` text NOT NULL, `operation_id` text NOT NULL, `receipt` text NOT NULL, PRIMARY KEY (`space_id`, `nonce`), CONSTRAINT `nonces_operation_body` FOREIGN KEY (`space_id`, `operation_id`, `request_body`) REFERENCES `operations` (`space_id`, `operation_id`, `request`));
