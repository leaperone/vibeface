-- Create "networks" table
CREATE TABLE `networks` (`network_id` text NULL, `revision` integer NOT NULL, `bundle` text NOT NULL, PRIMARY KEY (`network_id`));
-- Create "node_certificates" table
CREATE TABLE `node_certificates` (`network_id` text NOT NULL, `peer_id` text NOT NULL, `public_key` text NOT NULL, PRIMARY KEY (`network_id`, `peer_id`), CONSTRAINT `0` FOREIGN KEY (`network_id`) REFERENCES `networks` (`network_id`) ON UPDATE NO ACTION ON DELETE NO ACTION);
-- Create "local_identities" table
CREATE TABLE `local_identities` (`network_id` text NOT NULL, `peer_id` text NOT NULL, `identity` text NOT NULL, PRIMARY KEY (`network_id`, `peer_id`), CONSTRAINT `0` FOREIGN KEY (`network_id`) REFERENCES `networks` (`network_id`) ON UPDATE NO ACTION ON DELETE NO ACTION);
-- Create "events" table
CREATE TABLE `events` (`network_id` text NOT NULL, `hash` text NOT NULL, `scope_id` text NOT NULL, `author` text NOT NULL, `sequence` integer NOT NULL, `envelope` text NOT NULL, `status` text NOT NULL, PRIMARY KEY (`network_id`, `hash`), CONSTRAINT `0` FOREIGN KEY (`network_id`) REFERENCES `networks` (`network_id`) ON UPDATE NO ACTION ON DELETE NO ACTION);
-- Create index "events_author_sequence" to table: "events"
CREATE INDEX `events_author_sequence` ON `events` (`network_id`, `scope_id`, `author`, `sequence`);
-- Create index "events_scope" to table: "events"
CREATE INDEX `events_scope` ON `events` (`network_id`, `scope_id`);
-- Create "blobs" table
CREATE TABLE `blobs` (`network_id` text NOT NULL, `hash` text NOT NULL, `size` integer NOT NULL, `content` blob NOT NULL, PRIMARY KEY (`network_id`, `hash`), CONSTRAINT `0` FOREIGN KEY (`network_id`) REFERENCES `networks` (`network_id`) ON UPDATE NO ACTION ON DELETE NO ACTION);
-- Create "conversations" table
CREATE TABLE `conversations` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `title` text NOT NULL, PRIMARY KEY (`network_id`, `conversation_id`));
-- Create "messages" table
CREATE TABLE `messages` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `event_hash` text NOT NULL, `message_id` text NOT NULL, `role` text NOT NULL, `text` text NOT NULL, `created_at_ms` integer NOT NULL, `ordinal` integer NOT NULL, PRIMARY KEY (`network_id`, `event_hash`));
-- Create "tasks" table
CREATE TABLE `tasks` (`network_id` text NOT NULL, `conversation_id` text NOT NULL, `task_id` text NOT NULL, `title` text NOT NULL, `goal` text NOT NULL, `acceptance` text NOT NULL, `status` text NOT NULL, `detail` text NOT NULL, `event_hash` text NOT NULL, PRIMARY KEY (`network_id`, `conversation_id`, `task_id`));
-- Create "notification_outbox" table
CREATE TABLE `notification_outbox` (`network_id` text NOT NULL, `notification_id` text NOT NULL, `record` text NOT NULL, `derived` integer NOT NULL DEFAULT 0, PRIMARY KEY (`network_id`, `notification_id`));
-- Create "notification_acks" table
CREATE TABLE `notification_acks` (`network_id` text NOT NULL, `notification_id` text NOT NULL, `client_id` text NOT NULL, `event_hash` text NOT NULL DEFAULT '', PRIMARY KEY (`network_id`, `notification_id`, `client_id`));
-- Create "budget_days" table
CREATE TABLE `budget_days` (`network_id` text NOT NULL, `day` text NOT NULL, `used` integer NOT NULL, PRIMARY KEY (`network_id`, `day`));
-- Create "local_values" table
CREATE TABLE `local_values` (`network_id` text NOT NULL, `key` text NOT NULL, `value` text NOT NULL, PRIMARY KEY (`network_id`, `key`));
-- Create "identity_meta" table
CREATE TABLE `identity_meta` (`key` text NULL, `value` text NOT NULL, PRIMARY KEY (`key`));
-- Create "identity_requests" table
CREATE TABLE `identity_requests` (`request_id` text NULL, `subject_id` text NOT NULL, `kind` text NOT NULL, `response` text NOT NULL, `created_at_ms` integer NOT NULL, PRIMARY KEY (`request_id`));
-- Create "identity_invites" table
CREATE TABLE `identity_invites` (`invite_id` text NULL, `request_id` text NOT NULL, `network_id` text NOT NULL, `inviter_subject_id` text NOT NULL, `payload` text NOT NULL, `payload_hash` text NOT NULL, `capability_hash` text NOT NULL, `inviter_signature` text NOT NULL, `epoch` integer NOT NULL, `expires_at_ms` integer NOT NULL, `used` integer NOT NULL DEFAULT 0, PRIMARY KEY (`invite_id`));
-- Create index "identity_invites_request_id" to table: "identity_invites"
CREATE UNIQUE INDEX `identity_invites_request_id` ON `identity_invites` (`request_id`);
-- Create "identity_pair_reviews" table
CREATE TABLE `identity_pair_reviews` (`invite_id` text NULL, `review` text NOT NULL, `review_hash` text NOT NULL, `inviter_signature` text NOT NULL, `new_subject_signature` text NOT NULL, `created_at_ms` integer NOT NULL, PRIMARY KEY (`invite_id`));
-- Create "identity_pair_requests" table
CREATE TABLE `identity_pair_requests` (`request_id` text NULL, `invite_id` text NOT NULL, `subject_id` text NOT NULL, `review` text NOT NULL, `review_hash` text NOT NULL, `new_subject_signature` text NOT NULL DEFAULT '', PRIMARY KEY (`request_id`));
-- Create index "identity_pair_requests_invite_id" to table: "identity_pair_requests"
CREATE UNIQUE INDEX `identity_pair_requests_invite_id` ON `identity_pair_requests` (`invite_id`);

PRAGMA user_version = 5;
