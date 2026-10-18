schema "main" {}

table "schema_metadata" {
  schema = schema.main
  column "singleton" {
    type = integer
  }
  column "format_id" {
    type = text
  }
  column "data_epoch" {
    type = integer
  }
  column "layout_version" {
    type = integer
  }
  column "schema_fingerprint" {
    type = text
  }
  column "initialization_state" {
    type = text
  }
  primary_key {
    columns = [column.singleton]
  }
  check "metadata_singleton" {
    expr = "singleton = 1"
  }
  check "metadata_versions" {
    expr = "data_epoch > 0 AND layout_version > 0"
  }
  check "metadata_initialized" {
    expr = "initialization_state = 'ready'"
  }
}

table "spaces" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "revision" {
    type = integer
  }
  column "bundle" {
    type = text
  }
  primary_key {
    columns = [column.space_id]
  }
  check "space_revision" {
    expr = "revision >= 0"
  }
}

table "node_certificates" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "node_id" {
    type = text
  }
  column "public_key" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.node_id]
  }
  foreign_key "node_certificates_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
}

table "local_devices" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "device_id" {
    type = text
  }
  column "public_key" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.device_id]
  }
  foreign_key "local_devices_certificate" {
    columns = [column.space_id, column.device_id]
    ref_columns = [table.node_certificates.column.space_id, table.node_certificates.column.node_id]
  }
}

table "events" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "resource_id" {
    type = text
  }
  column "hash" {
    type = text
  }
  column "scope_id" {
    type = text
  }
  column "author" {
    type = text
  }
  column "sequence" {
    type = integer
  }
  column "envelope" {
    type = text
  }
  column "status" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.hash]
  }
  foreign_key "events_author" {
    columns = [column.space_id, column.author]
    ref_columns = [table.node_certificates.column.space_id, table.node_certificates.column.node_id]
  }
  index "events_author_sequence" {
    columns = [column.space_id, column.scope_id, column.author, column.sequence]
  }
  index "events_scope" {
    columns = [column.space_id, column.scope_id]
  }
  check "event_sequence" {
    expr = "sequence > 0"
  }
  check "event_status" {
    expr = "status IN ('pending', 'ready', 'conflict', 'invalid')"
  }
  foreign_key "events_resource" {
    columns = [column.space_id, column.resource_id]
    ref_columns = [table.resource_records.column.space_id, table.resource_records.column.resource_id]
  }
}

table "blobs" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "hash" {
    type = text
  }
  column "size" {
    type = integer
  }
  column "content" {
    type = blob
  }
  primary_key {
    columns = [column.space_id, column.hash]
  }
  foreign_key "blobs_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  check "blob_nonnegative_size" {
    expr = "size >= 0"
  }
}

table "conversations" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "resource_id" {
    type = text
  }
  column "conversation_id" {
    type = text
  }
  column "title" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.conversation_id]
  }
  foreign_key "conversations_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  foreign_key "conversations_resource" {
    columns = [column.space_id, column.resource_id]
    ref_columns = [table.resource_records.column.space_id, table.resource_records.column.resource_id]
  }
}

table "messages" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "conversation_id" {
    type = text
  }
  column "event_hash" {
    type = text
  }
  column "message_id" {
    type = text
  }
  column "role" {
    type = text
  }
  column "text" {
    type = text
  }
  column "created_at_ms" {
    type = integer
  }
  column "ordinal" {
    type = integer
  }
  primary_key {
    columns = [column.space_id, column.event_hash]
  }
  foreign_key "messages_source" {
    columns = [column.space_id, column.event_hash]
    ref_columns = [table.events.column.space_id, table.events.column.hash]
  }
}

table "tasks" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "conversation_id" {
    type = text
  }
  column "task_id" {
    type = text
  }
  column "title" {
    type = text
  }
  column "goal" {
    type = text
  }
  column "acceptance" {
    type = text
  }
  column "status" {
    type = text
  }
  column "detail" {
    type = text
  }
  column "event_hash" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.conversation_id, column.task_id]
  }
  foreign_key "tasks_source" {
    columns = [column.space_id, column.event_hash]
    ref_columns = [table.events.column.space_id, table.events.column.hash]
  }
}

table "execution_sessions" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "target_node" {
    type = text
  }
  column "session_id" {
    type = text
  }
  column "revision" {
    type = integer
  }
  column "record" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.target_node, column.session_id]
  }
  foreign_key "sessions_node" {
    columns = [column.space_id, column.target_node]
    ref_columns = [table.node_certificates.column.space_id, table.node_certificates.column.node_id]
  }
  check "session_revision" {
    expr = "revision >= 0"
  }
}

table "operations" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "operation_id" {
    type = text
  }
  column "target_node" {
    type = text
  }
  column "session_id" {
    type = text
    null = true
  }
  column "request" {
    type = text
  }
  column "state" {
    type = text
  }
  column "control" {
    type = integer
    default = 0
  }
  column "result" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.operation_id]
  }
  foreign_key "operations_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  index "operations_session" {
    columns = [column.space_id, column.target_node, column.session_id, column.state]
  }
  index "operations_request_reference" {
    unique = true
    columns = [column.space_id, column.operation_id, column.request]
  }
  check "operation_control" {
    expr = "control IN (0,1)"
  }
}

table "resource_reservations" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "operation_id" {
    type = text
  }
  column "target_node" {
    type = text
  }
  column "resource" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.operation_id, column.resource]
  }
  foreign_key "reservations_operation" {
    columns = [column.space_id, column.operation_id]
    ref_columns = [table.operations.column.space_id, table.operations.column.operation_id]
  }
}

table "continuation_claims" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "result_event_hash" {
    type = text
  }
  column "policy_version" {
    type = text
  }
  column "record" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.result_event_hash, column.policy_version]
  }
  foreign_key "continuations_result" {
    columns = [column.space_id, column.result_event_hash]
    ref_columns = [table.events.column.space_id, table.events.column.hash]
  }
}

table "notification_outbox" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "notification_id" {
    type = text
  }
  column "record" {
    type = text
  }
  column "derived" {
    type = integer
    default = 0
  }
  primary_key {
    columns = [column.space_id, column.notification_id]
  }
  foreign_key "notification_outbox_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  check "notification_derived" {
    expr = "derived IN (0,1)"
  }
}

table "notification_acks" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "notification_id" {
    type = text
  }
  column "client_id" {
    type = text
  }
  column "event_hash" {
    type = text
    default = ""
  }
  primary_key {
    columns = [column.space_id, column.notification_id, column.client_id]
  }
  foreign_key "notification_acks_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
}

table "budget_days" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "day" {
    type = text
  }
  column "used" {
    type = integer
  }
  primary_key {
    columns = [column.space_id, column.day]
  }
  foreign_key "budget_days_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  check "budget_nonnegative" {
    expr = "used >= 0"
  }
}

table "local_values" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "key" {
    type = text
  }
  column "value" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.key]
  }
  foreign_key "local_values_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
}

table "identity_meta" {
  schema = schema.main
  column "key" {
    type = text
  }
  column "value" {
    type = text
  }
  primary_key {
    columns = [column.key]
  }
}

table "event_dependencies" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "child_hash" {
    type = text
  }
  column "parent_hash" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.child_hash, column.parent_hash]
  }
  foreign_key "dependencies_child" {
    columns = [column.space_id, column.child_hash]
    ref_columns = [table.events.column.space_id, table.events.column.hash]
  }
}

table "event_blobs" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "event_hash" {
    type = text
  }
  column "blob_hash" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.event_hash, column.blob_hash]
  }
  foreign_key "event_blobs_event" {
    columns = [column.space_id, column.event_hash]
    ref_columns = [table.events.column.space_id, table.events.column.hash]
  }
}

table "nonce_receipts" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "nonce" {
    type = text
  }
  column "body_hash" {
    type = text
  }
  column "request_body" {
    type = text
  }
  column "operation_id" {
    type = text
  }
  column "receipt" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.nonce]
  }
  foreign_key "nonces_operation_body" {
    columns = [column.space_id, column.operation_id, column.request_body]
    ref_columns = [table.operations.column.space_id, table.operations.column.operation_id, table.operations.column.request]
  }
}

table "policy_pins" {
  schema = schema.main
  column "domain_kind" {
    type = text
  }
  column "domain_id" {
    type = text
  }
  column "genesis_root" {
    type = text
  }
  column "minimum_head" {
    type = text
    null = true
  }
  primary_key {
    columns = [column.domain_kind, column.domain_id]
  }
  check "policy_pins_kind" {
    expr = "domain_kind IN ('account', 'space')"
  }
}

table "policy_journal" {
  schema = schema.main
  column "ordinal" {
    type = integer
  }
  column "entry_hash" {
    type = text
  }
  column "entry" {
    type = text
  }
  column "accepted_at_ms" {
    type = integer
  }
  primary_key {
    columns = [column.ordinal]
  }
  index "policy_journal_entry_hash" {
    unique = true
    columns = [column.entry_hash]
  }
  check "policy_journal_accepted_at" {
    expr = "accepted_at_ms >= 0"
  }
}

table "current_policy_heads" {
  schema = schema.main
  column "domain_kind" {
    type = text
  }
  column "domain_id" {
    type = text
  }
  column "authority_epoch" {
    type = integer
  }
  column "sequence" {
    type = integer
  }
  column "head_hash" {
    type = text
  }
  primary_key {
    columns = [column.domain_kind, column.domain_id]
  }
  check "current_policy_heads_kind" {
    expr = "domain_kind IN ('account', 'space')"
  }
  check "current_policy_heads_version" {
    expr = "authority_epoch >= 0 AND sequence >= 0"
  }
}

table "authorization_challenges" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "nonce" {
    type = text
  }
  column "invocation_hash" {
    type = text
  }
  column "stage" {
    type = text
  }
  column "challenge" {
    type = text
  }
  column "expires_at_ms" {
    type = integer
  }
  column "consumed" {
    type = integer
    default = 0
  }
  primary_key {
    columns = [column.space_id, column.nonce]
  }
  foreign_key "authorization_challenges_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  check "authorization_challenges_stage" {
    expr = "stage IN ('remote_read', 'admit', 'start', 'renew')"
  }
  check "authorization_challenges_expiry" {
    expr = "expires_at_ms >= 0"
  }
  check "authorization_challenges_consumed" {
    expr = "consumed IN (0,1)"
  }
}

table "accepted_authorizations" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "authorization_id" {
    type = text
  }
  column "invocation_hash" {
    type = text
  }
  column "invocation_body_hash" {
    type = text
  }
  column "actor_kind" {
    type = text
  }
  column "actor_id" {
    type = text
  }
  column "device_id" {
    type = text
  }
  column "resource_id" {
    type = text
  }
  column "action" {
    type = text
  }
  column "grant_leaf" {
    type = text
  }
  column "signed_invocation" {
    type = text
  }
  column "source_body" {
    type = text
  }
  column "transport" {
    type = text
  }
  column "admission_challenge" {
    type = text
  }
  column "admission_receipts" {
    type = text
  }
  column "admitted_at_ms" {
    type = integer
  }
  column "operation_id" {
    type = text
    null = true
  }
  primary_key {
    columns = [column.space_id, column.authorization_id]
  }
  index "accepted_authorizations_invocation" {
    unique = true
    columns = [column.space_id, column.invocation_hash]
  }
  foreign_key "accepted_authorizations_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  check "accepted_authorizations_actor" {
    expr = "actor_kind IN ('account', 'service')"
  }
  check "accepted_authorizations_admitted_at" {
    expr = "admitted_at_ms >= 0"
  }
}

table "authorization_nonces" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "device_id" {
    type = text
  }
  column "nonce" {
    type = text
  }
  column "authorization_id" {
    type = text
  }
  column "body_hash" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.device_id, column.nonce]
  }
  foreign_key "authorization_nonces_authorization" {
    columns = [column.space_id, column.authorization_id]
    ref_columns = [table.accepted_authorizations.column.space_id, table.accepted_authorizations.column.authorization_id]
  }
}

table "resource_records" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "resource_id" {
    type = text
  }
  column "record_hash" {
    type = text
  }
  column "kind" {
    type = text
  }
  column "host_service_id" {
    type = text
  }
  column "signed_record" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.resource_id]
  }
  index "resource_records_hash" {
    unique = true
    columns = [column.space_id, column.record_hash]
  }
  foreign_key "resource_records_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  check "resource_records_kind" {
    expr = "kind IN ('space_control', 'project', 'execution_service')"
  }
}

table "policy_nonces" {
  schema = schema.main
  column "domain_kind" {
    type = text
  }
  column "domain_id" {
    type = text
  }
  column "nonce" {
    type = text
  }
  column "entry_hash" {
    type = text
  }
  primary_key {
    columns = [column.domain_kind, column.domain_id, column.nonce]
  }
  foreign_key "policy_nonces_entry" {
    columns = [column.entry_hash]
    ref_columns = [table.policy_journal.column.entry_hash]
  }
  check "policy_nonces_kind" {
    expr = "domain_kind IN ('account', 'space')"
  }
}

table "passkey_pending" {
  schema = schema.main
  column "account_id" {
    type = text
  }
  column "envelope_hash" {
    type = text
  }
  column "intent_hash" {
    type = text
  }
  column "nonce" {
    type = text
  }
  column "pending" {
    type = text
  }
  column "expires_at_ms" {
    type = integer
  }
  primary_key {
    columns = [column.account_id, column.envelope_hash]
  }
  index "passkey_pending_account_nonce" {
    unique = true
    columns = [column.account_id, column.nonce]
  }
  index "passkey_pending_account_intent" {
    unique = true
    columns = [column.account_id, column.intent_hash]
  }
  check "passkey_pending_expiry" {
    expr = "expires_at_ms >= 0"
  }
}

table "passkey_authenticators" {
  schema = schema.main
  column "account_id" {
    type = text
  }
  column "credential_id" {
    type = text
  }
  column "rp_id" {
    type = text
  }
  column "record_hash" {
    type = text
  }
  column "sign_count" {
    type = integer
  }
  column "backup_state" {
    type = integer
  }
  column "last_envelope_hash" {
    type = text
  }
  primary_key {
    columns = [column.account_id, column.credential_id]
  }
  index "passkey_authenticators_rp_credential" {
    unique = true
    columns = [column.rp_id, column.credential_id]
  }
  index "passkey_authenticators_record" {
    unique = true
    columns = [column.record_hash]
  }
  foreign_key "passkey_authenticators_pending" {
    columns = [column.account_id, column.last_envelope_hash]
    ref_columns = [table.passkey_pending.column.account_id, table.passkey_pending.column.envelope_hash]
  }
  check "passkey_authenticators_counter" {
    expr = "sign_count BETWEEN 0 AND 4294967295"
  }
  check "passkey_authenticators_backup" {
    expr = "backup_state IN (0, 1)"
  }
}

table "passkey_receipts" {
  schema = schema.main
  column "account_id" {
    type = text
  }
  column "envelope_hash" {
    type = text
  }
  column "completion_hash" {
    type = text
  }
  column "receipt" {
    type = text
  }
  column "accepted_at_ms" {
    type = integer
  }
  primary_key {
    columns = [column.account_id, column.envelope_hash]
  }
  foreign_key "passkey_receipts_pending" {
    columns = [column.account_id, column.envelope_hash]
    ref_columns = [table.passkey_pending.column.account_id, table.passkey_pending.column.envelope_hash]
  }
  check "passkey_receipts_accepted_at" {
    expr = "accepted_at_ms >= 0"
  }
}

table "execution_binding_seals" {
  schema = schema.main
  column "original_space_id" {
    type = text
  }
  column "original_resource_id" {
    type = text
  }
  column "task_ref" {
    type = text
  }
  column "target_node" {
    type = text
  }
  column "generation" {
    type = integer
  }
  column "execution_space_id" {
    type = text
  }
  column "execution_resource_id" {
    type = text
  }
  column "binding_hash" {
    type = text
  }
  column "seal_invocation_hash" {
    type = text
  }
  column "admission" {
    type = text
  }
  column "seal" {
    type = text
  }
  primary_key {
    columns = [column.original_space_id, column.original_resource_id, column.task_ref, column.target_node, column.generation]
  }
  foreign_key "binding_seals_node" {
    columns     = [column.execution_space_id, column.target_node]
    ref_columns = [table.node_certificates.column.space_id, table.node_certificates.column.node_id]
  }
  foreign_key "binding_seals_space" {
    columns     = [column.execution_space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  index "binding_seals_invocation_once" {
    unique  = true
    columns = [column.execution_space_id, column.seal_invocation_hash]
  }
  check "binding_seals_generation" {
    expr = "generation >= 1"
  }
  check "binding_seals_admission_json" {
    expr = "json_valid(admission)"
  }
  check "binding_seals_seal_json" {
    expr = "json_valid(seal)"
  }
}

table "execution_handoffs" {
  schema = schema.main
  column "execution_space_id" {
    type = text
  }
  column "original_space_id" {
    type = text
  }
  column "operation_id" {
    type = text
  }
  column "target_node" {
    type = text
  }
  column "session_id" {
    type = text
  }
  column "original_request" {
    type = text
  }
  column "handoff" {
    type = text
  }
  column "handoff_hash" {
    type = text
  }
  column "invocation_hash" {
    type = text
  }
  column "body_hash" {
    type = text
  }
  column "admission_transport" {
    type = text
  }
  column "admission_challenge" {
    type = text
  }
  column "admission_receipts" {
    type = text
  }
  column "admission_receipt" {
    type = text
  }
  column "admitted_at_ms" {
    type = integer
  }
  column "state" {
    type = text
  }
  column "control" {
    type = integer
  }
  column "result" {
    type = text
  }
  column "lease_challenge" {
    null = true
    type = text
  }
  column "lease_receipts" {
    null = true
    type = text
  }
  column "lease_verified_at_ms" {
    null = true
    type = integer
  }
  column "lease_deadline_ms" {
    null = true
    type = integer
  }
  column "result_receipt" {
    null = true
    type = text
  }
  primary_key {
    columns = [column.execution_space_id, column.original_space_id, column.operation_id]
  }
  foreign_key "execution_handoffs_fk_0" {
    columns     = [column.execution_space_id, column.target_node]
    ref_columns = [table.node_certificates.column.space_id, table.node_certificates.column.node_id]
  }
  foreign_key "execution_handoffs_fk_1" {
    columns     = [column.original_space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  foreign_key "execution_handoffs_fk_2" {
    columns     = [column.execution_space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  index "handoffs_dispatch_once" {
    unique  = true
    columns = [column.execution_space_id, column.handoff_hash]
  }
  index "handoffs_invocation_once" {
    unique  = true
    columns = [column.execution_space_id, column.invocation_hash]
  }
  index "handoffs_original_operation_once" {
    unique  = true
    columns = [column.original_space_id, column.operation_id]
  }
  check {
    expr = "json_valid(admission_receipt)"
  }
  check {
    expr = "(admitted_at_ms >= 0)"
  }
  check {
    expr = "(state IN ('accepted','running','unknown','succeeded','failed','rejected'))"
  }
  check {
    expr = "(control IN (0,1))"
  }
  check {
    expr = "((lease_challenge IS NULL AND lease_receipts IS NULL AND lease_verified_at_ms IS NULL AND lease_deadline_ms IS NULL)\n     OR (lease_challenge IS NOT NULL AND lease_receipts IS NOT NULL\n         AND lease_verified_at_ms IS NOT NULL AND lease_deadline_ms IS NOT NULL\n         AND lease_verified_at_ms >= 0 AND lease_deadline_ms > lease_verified_at_ms))"
  }
}

table "execution_handoff_challenges" {
  schema = schema.main
  column "execution_space_id" {
    type = text
  }
  column "nonce" {
    type = text
  }
  column "original_space_id" {
    type = text
  }
  column "operation_id" {
    type = text
  }
  column "handoff_hash" {
    type = text
  }
  column "stage" {
    type = text
  }
  column "challenge" {
    type = text
  }
  column "expires_at_ms" {
    type = integer
  }
  column "consumed" {
    type    = integer
    default = 0
  }
  primary_key {
    columns = [column.execution_space_id, column.nonce]
  }
  foreign_key "execution_handoff_challenges_fk_0" {
    columns     = [column.original_space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  foreign_key "execution_handoff_challenges_fk_1" {
    columns     = [column.execution_space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  check {
    expr = "(stage IN ('admit','start','renew','remote_read'))"
  }
  check {
    expr = "(expires_at_ms >= 0)"
  }
  check {
    expr = "(consumed IN (0,1))"
  }
}

table "execution_handoff_nonces" {
  schema = schema.main
  column "execution_space_id" {
    type = text
  }
  column "device_id" {
    type = text
  }
  column "nonce" {
    type = text
  }
  column "original_space_id" {
    type = text
  }
  column "operation_id" {
    type = text
  }
  column "invocation_hash" {
    type = text
  }
  column "body_hash" {
    type = text
  }
  primary_key {
    columns = [column.execution_space_id, column.device_id, column.nonce]
  }
  foreign_key "execution_handoff_nonces_fk_0" {
    columns     = [column.execution_space_id, column.original_space_id, column.operation_id]
    ref_columns = [table.execution_handoffs.column.execution_space_id, table.execution_handoffs.column.original_space_id, table.execution_handoffs.column.operation_id]
  }
  index "handoff_nonce_operation_once" {
    unique  = true
    columns = [column.execution_space_id, column.original_space_id, column.operation_id]
  }
}

table "execution_handoff_reservations" {
  schema = schema.main
  column "execution_space_id" {
    type = text
  }
  column "original_space_id" {
    type = text
  }
  column "operation_id" {
    type = text
  }
  column "target_node" {
    type = text
  }
  column "resource" {
    type = text
  }
  primary_key {
    columns = [column.execution_space_id, column.original_space_id, column.operation_id, column.resource]
  }
  foreign_key "execution_handoff_reservations_fk_0" {
    columns     = [column.execution_space_id, column.original_space_id, column.operation_id]
    ref_columns = [table.execution_handoffs.column.execution_space_id, table.execution_handoffs.column.original_space_id, table.execution_handoffs.column.operation_id]
  }
}

table "execution_handoff_sessions" {
  schema = schema.main
  column "execution_space_id" {
    type = text
  }
  column "original_space_id" {
    type = text
  }
  column "original_resource_id" {
    type = text
  }
  column "target_node" {
    type = text
  }
  column "session_id" {
    type = text
  }
  column "revision" {
    type = integer
  }
  column "binding" {
    type = text
  }
  column "record" {
    type = text
  }
  primary_key {
    columns = [column.execution_space_id, column.original_space_id, column.original_resource_id, column.target_node, column.session_id]
  }
  foreign_key "execution_handoff_sessions_fk_0" {
    columns     = [column.execution_space_id, column.target_node]
    ref_columns = [table.node_certificates.column.space_id, table.node_certificates.column.node_id]
  }
  foreign_key "execution_handoff_sessions_fk_1" {
    columns     = [column.original_space_id, column.original_resource_id]
    ref_columns = [table.resource_records.column.space_id, table.resource_records.column.resource_id]
  }
  foreign_key "execution_handoff_sessions_fk_2" {
    columns     = [column.original_space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  check {
    expr = "(revision >= 0)"
  }
}

table "execution_handoff_resolution_challenges" {
  schema = schema.main
  column "execution_space_id" {
    type = text
  }
  column "nonce" {
    type = text
  }
  column "original_space_id" {
    type = text
  }
  column "operation_id" {
    type = text
  }
  column "resolution_id" {
    type = text
  }
  column "resolution_hash" {
    type = text
  }
  column "challenge" {
    type = text
  }
  column "expires_at_ms" {
    type = integer
  }
  column "consumed" {
    type    = integer
    default = 0
  }
  primary_key {
    columns = [column.execution_space_id, column.nonce]
  }
  foreign_key "execution_handoff_resolution_challenges_fk_0" {
    columns     = [column.execution_space_id, column.original_space_id, column.operation_id]
    ref_columns = [table.execution_handoffs.column.execution_space_id, table.execution_handoffs.column.original_space_id, table.execution_handoffs.column.operation_id]
  }
  check {
    expr = "(expires_at_ms >= 0)"
  }
  check {
    expr = "(consumed IN (0,1))"
  }
}

table "execution_handoff_resolutions" {
  schema = schema.main
  column "execution_space_id" {
    type = text
  }
  column "original_space_id" {
    type = text
  }
  column "operation_id" {
    type = text
  }
  column "resolution_id" {
    type = text
  }
  column "resolution_hash" {
    type = text
  }
  column "resolver_device" {
    type = text
  }
  column "resolver_nonce" {
    type = text
  }
  column "resolver_invocation_hash" {
    type = text
  }
  column "resolver_body_hash" {
    type = text
  }
  column "submission_device" {
    type = text
  }
  column "submission_nonce" {
    type = text
  }
  column "submission_invocation_hash" {
    type = text
  }
  column "submission_body_hash" {
    type = text
  }
  column "request" {
    type = text
  }
  column "admission_transport" {
    type = text
  }
  column "freshness" {
    type = text
  }
  column "resolved_at_ms" {
    type = integer
  }
  column "result_receipt" {
    type = text
  }
  primary_key {
    columns = [column.execution_space_id, column.original_space_id, column.operation_id]
  }
  foreign_key "execution_handoff_resolutions_fk_0" {
    columns     = [column.execution_space_id, column.original_space_id, column.operation_id]
    ref_columns = [table.execution_handoffs.column.execution_space_id, table.execution_handoffs.column.original_space_id, table.execution_handoffs.column.operation_id]
  }
  index "handoff_resolution_id_once" {
    unique  = true
    columns = [column.original_space_id, column.resolution_id]
  }
  index "handoff_resolution_resolver_nonce_once" {
    unique  = true
    columns = [column.execution_space_id, column.resolver_device, column.resolver_nonce]
  }
  index "handoff_resolution_submission_nonce_once" {
    unique  = true
    columns = [column.execution_space_id, column.submission_device, column.submission_nonce]
  }
  check {
    expr = "(resolved_at_ms >= 0)"
  }
}
