table "networks" {
  schema = schema.main
  column "network_id" {
    null = true
    type = text
  }
  column "revision" {
    null = false
    type = integer
  }
  column "bundle" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id]
  }
}
table "node_certificates" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "peer_id" {
    null = false
    type = text
  }
  column "public_key" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.peer_id]
  }
  foreign_key "0" {
    columns     = [column.network_id]
    ref_columns = [table.networks.column.network_id]
    on_update   = NO_ACTION
    on_delete   = NO_ACTION
  }
}
table "local_identities" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "peer_id" {
    null = false
    type = text
  }
  column "identity" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.peer_id]
  }
  foreign_key "0" {
    columns     = [column.network_id]
    ref_columns = [table.networks.column.network_id]
    on_update   = NO_ACTION
    on_delete   = NO_ACTION
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
  column "envelope" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.author, column.seq]
  }
  foreign_key "0" {
    columns     = [column.network_id]
    ref_columns = [table.networks.column.network_id]
    on_update   = NO_ACTION
    on_delete   = NO_ACTION
  }
  index "events_hash" {
    unique  = true
    columns = [column.network_id, column.hash]
  }
  index "events_order" {
    columns = [column.network_id, column.scope_id, column.lamport, column.created_at_ms, column.hash]
  }
  index "events_scope_author" {
    columns = [column.network_id, column.scope_id, column.author, column.seq]
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
  column "author" {
    null = false
    type = text
  }
  column "seq" {
    null = false
    type = integer
  }
  column "envelope" {
    null = false
    type = text
  }
  column "missing" {
    null = false
    type = text
  }
  column "received_at_ms" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.network_id, column.hash]
  }
  index "pending_events_missing" {
    columns = [column.network_id, column.missing]
  }
}
table "log_conflicts" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "hash" {
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
  column "envelope" {
    null = false
    type = text
  }
  column "kept_hash" {
    null = false
    type = text
  }
  column "detected_at_ms" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.network_id, column.hash]
  }
  index "log_conflicts_author" {
    columns = [column.network_id, column.author, column.seq]
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
  column "owner_hash" {
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
  column "blobs" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.conversation_id, column.message_id]
  }
  index "messages_order" {
    columns = [column.network_id, column.conversation_id, column.rank]
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
  primary_key {
    columns = [column.network_id, column.conversation_id]
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
table "approvals" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "request_id" {
    null = false
    type = text
  }
  column "work_id" {
    null = false
    type = text
  }
  column "kind" {
    null = true
    type = text
  }
  column "summary" {
    null = true
    type = text
  }
  column "request_rank" {
    null = true
    type = text
  }
  column "decision" {
    null = true
    type = text
  }
  column "answer_rank" {
    null = true
    type = text
  }
  primary_key {
    columns = [column.network_id, column.request_id]
  }
  index "approvals_work" {
    columns = [column.network_id, column.work_id]
  }
}
table "autonomy" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "conversation_id" {
    null = false
    type = text
  }
  column "mode" {
    null = false
    type = text
  }
  column "strictness" {
    null = false
    type = integer
  }
  column "version" {
    null = false
    type = integer
  }
  column "event_hash" {
    null = false
    type = text
  }
  primary_key {
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
table "local_values" {
  schema = schema.main
  column "network_id" {
    null = false
    type = text
  }
  column "key" {
    null = false
    type = text
  }
  column "value" {
    null = false
    type = text
  }
  primary_key {
    columns = [column.network_id, column.key]
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
table "identity_requests" {
  schema = schema.main
  column "request_id" {
    null = true
    type = text
  }
  column "subject_id" {
    null = false
    type = text
  }
  column "kind" {
    null = false
    type = text
  }
  column "response" {
    null = false
    type = text
  }
  column "created_at_ms" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.request_id]
  }
}
table "identity_invites" {
  schema = schema.main
  column "invite_id" {
    null = true
    type = text
  }
  column "request_id" {
    null = false
    type = text
  }
  column "network_id" {
    null = false
    type = text
  }
  column "inviter_subject_id" {
    null = false
    type = text
  }
  column "payload" {
    null = false
    type = text
  }
  column "payload_hash" {
    null = false
    type = text
  }
  column "capability_hash" {
    null = false
    type = text
  }
  column "inviter_signature" {
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
  column "used" {
    null    = false
    type    = integer
    default = 0
  }
  primary_key {
    columns = [column.invite_id]
  }
  index "identity_invites_request_id" {
    unique  = true
    columns = [column.request_id]
  }
}
table "identity_pair_reviews" {
  schema = schema.main
  column "invite_id" {
    null = true
    type = text
  }
  column "review" {
    null = false
    type = text
  }
  column "review_hash" {
    null = false
    type = text
  }
  column "inviter_signature" {
    null = false
    type = text
  }
  column "new_subject_signature" {
    null = false
    type = text
  }
  column "created_at_ms" {
    null = false
    type = integer
  }
  primary_key {
    columns = [column.invite_id]
  }
}
table "identity_pair_requests" {
  schema = schema.main
  column "request_id" {
    null = true
    type = text
  }
  column "invite_id" {
    null = false
    type = text
  }
  column "subject_id" {
    null = false
    type = text
  }
  column "review" {
    null = false
    type = text
  }
  column "review_hash" {
    null = false
    type = text
  }
  column "new_subject_signature" {
    null    = false
    type    = text
    default = ""
  }
  primary_key {
    columns = [column.request_id]
  }
  index "identity_pair_requests_invite_id" {
    unique  = true
    columns = [column.invite_id]
  }
}
schema "main" {
}
