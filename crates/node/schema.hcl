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
    expr = "revision > 0"
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
  foreign_key "operations_node" {
    columns = [column.space_id, column.target_node]
    ref_columns = [table.node_certificates.column.space_id, table.node_certificates.column.node_id]
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

table "identity_requests" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "request_id" {
    type = text
  }
  column "subject_id" {
    type = text
  }
  column "kind" {
    type = text
  }
  column "response" {
    type = text
  }
  column "created_at_ms" {
    type = integer
  }
  primary_key {
    columns = [column.space_id, column.request_id]
  }
  foreign_key "identity_requests_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
}

table "identity_invites" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "invite_id" {
    type = text
  }
  column "request_id" {
    type = text
  }
  column "inviter_subject_id" {
    type = text
  }
  column "payload" {
    type = text
  }
  column "payload_hash" {
    type = text
  }
  column "capability_hash" {
    type = text
  }
  column "inviter_signature" {
    type = text
  }
  column "epoch" {
    type = integer
  }
  column "expires_at_ms" {
    type = integer
  }
  column "used" {
    type = integer
    default = 0
  }
  primary_key {
    columns = [column.space_id, column.invite_id]
  }
  foreign_key "identity_invites_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  index "identity_invites_request" {
    unique = true
    columns = [column.space_id, column.request_id]
  }

  check "identity_invites_used" {
    expr = "used IN (0,1)"
  }
  check "identity_invites_epoch" {
    expr = "epoch > 0"
  }
}

table "identity_pair_requests" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "request_id" {
    type = text
  }
  column "invite_id" {
    type = text
  }
  column "subject_id" {
    type = text
  }
  column "review" {
    type = text
  }
  column "review_hash" {
    type = text
  }
  column "new_subject_signature" {
    type = text
    default = ""
  }
  primary_key {
    columns = [column.space_id, column.request_id]
  }
  foreign_key "pair_requests_invite" {
    columns = [column.space_id, column.invite_id]
    ref_columns = [table.identity_invites.column.space_id, table.identity_invites.column.invite_id]
  }
  index "pair_requests_invite_once" {
    unique = true
    columns = [column.space_id, column.invite_id]
  }
}

table "identity_pair_reviews" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "invite_id" {
    type = text
  }
  column "review" {
    type = text
  }
  column "review_hash" {
    type = text
  }
  column "inviter_signature" {
    type = text
  }
  column "new_subject_signature" {
    type = text
  }
  column "created_at_ms" {
    type = integer
  }
  primary_key {
    columns = [column.space_id, column.invite_id]
  }
  foreign_key "pair_reviews_request" {
    columns = [column.space_id, column.invite_id]
    ref_columns = [table.identity_pair_requests.column.space_id, table.identity_pair_requests.column.invite_id]
  }
}

table "accounts" {
  schema = schema.main
  column "account_id" {
    type = text
  }
  column "root_public_key" {
    type = text
  }
  column "public_record" {
    type = text
  }
  primary_key {
    columns = [column.account_id]
  }
}

table "devices" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "device_id" {
    type = text
  }
  column "account_id" {
    type = text
  }
  column "kind" {
    type = text
  }
  column "public_key" {
    type = text
  }
  column "certificate" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.device_id]
  }
  foreign_key "devices_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  foreign_key "devices_account" {
    columns = [column.account_id]
    ref_columns = [table.accounts.column.account_id]
  }
  check "device_kind" {
    expr = "kind IN ('node', 'client')"
  }
}

table "members" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "account_id" {
    type = text
  }
  column "role" {
    type = text
  }
  column "signed_record" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.account_id]
  }
  foreign_key "members_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  foreign_key "members_account" {
    columns = [column.account_id]
    ref_columns = [table.accounts.column.account_id]
  }
  check "member_role" {
    expr = "role IN ('owner', 'admin', 'member')"
  }
}

table "resources" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "resource_id" {
    type = text
  }
  column "owner_account_id" {
    type = text
  }
  column "host_node_id" {
    type = text
  }
  column "kind" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.resource_id]
  }
  foreign_key "resources_owner" {
    columns = [column.owner_account_id]
    ref_columns = [table.accounts.column.account_id]
  }
  foreign_key "resources_host" {
    columns = [column.space_id, column.host_node_id]
    ref_columns = [table.devices.column.space_id, table.devices.column.device_id]
  }
  check "resource_kind" {
    expr = "kind IN ('agent', 'relay', 'storage')"
  }
}

table "grants" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "grant_id" {
    type = text
  }
  column "resource_id" {
    type = text
  }
  column "grantee_account_id" {
    type = text
  }
  column "parent_grant_id" {
    type = text
    null = true
  }
  column "action" {
    type = text
  }
  column "signed_envelope" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.grant_id]
  }
  foreign_key "grants_resource" {
    columns = [column.space_id, column.resource_id]
    ref_columns = [table.resources.column.space_id, table.resources.column.resource_id]
  }
  foreign_key "grants_grantee" {
    columns = [column.grantee_account_id]
    ref_columns = [table.accounts.column.account_id]
  }
  foreign_key "grants_parent" {
    columns = [column.space_id, column.parent_grant_id]
    ref_columns = [table.grants.column.space_id, table.grants.column.grant_id]
  }
}

table "policy_events" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "hash" {
    type = text
  }
  column "authority_epoch" {
    type = integer
  }
  column "sequence" {
    type = integer
  }
  column "signed_envelope" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.hash]
  }
  index "policy_accepted_sequence" {
    unique = true
    columns = [column.space_id, column.authority_epoch, column.sequence]
  }
  index "policy_head_reference" {
    unique = true
    columns = [column.space_id, column.authority_epoch, column.sequence, column.hash]
  }
  foreign_key "policy_events_space" {
    columns = [column.space_id]
    ref_columns = [table.spaces.column.space_id]
  }
  check "policy_monotonic_values" {
    expr = "authority_epoch > 0 AND sequence > 0"
  }
}

table "policy_heads" {
  schema = schema.main
  column "space_id" {
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
    columns = [column.space_id]
  }
  foreign_key "policy_heads_event" {
    columns = [column.space_id, column.authority_epoch, column.sequence, column.head_hash]
    ref_columns = [table.policy_events.column.space_id, table.policy_events.column.authority_epoch, table.policy_events.column.sequence, table.policy_events.column.hash]
  }
}

table "authorization_decisions" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "operation_id" {
    type = text
  }
  column "policy_hash" {
    type = text
  }
  column "actor_account_id" {
    type = text
  }
  column "receipt" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.operation_id]
  }
  foreign_key "decisions_operation" {
    columns = [column.space_id, column.operation_id]
    ref_columns = [table.operations.column.space_id, table.operations.column.operation_id]
  }
  foreign_key "decisions_policy" {
    columns = [column.space_id, column.policy_hash]
    ref_columns = [table.policy_events.column.space_id, table.policy_events.column.hash]
  }
  foreign_key "decisions_actor" {
    columns = [column.actor_account_id]
    ref_columns = [table.accounts.column.account_id]
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

table "resource_claims" {
  schema = schema.main
  column "space_id" {
    type = text
  }
  column "resource_id" {
    type = text
  }
  column "operation_id" {
    type = text
  }
  primary_key {
    columns = [column.space_id, column.resource_id]
  }
  foreign_key "claims_resource" {
    columns = [column.space_id, column.resource_id]
    ref_columns = [table.resources.column.space_id, table.resources.column.resource_id]
  }
  foreign_key "claims_operation" {
    columns = [column.space_id, column.operation_id]
    ref_columns = [table.operations.column.space_id, table.operations.column.operation_id]
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
