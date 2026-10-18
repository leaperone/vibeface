CREATE TABLE networks (
  network_id TEXT PRIMARY KEY,
  revision INTEGER NOT NULL,
  bundle TEXT NOT NULL
);
CREATE TABLE node_certificates (
  network_id TEXT NOT NULL,
  node_id TEXT NOT NULL,
  public_key TEXT NOT NULL,
  PRIMARY KEY(network_id,node_id),
  FOREIGN KEY(network_id) REFERENCES networks(network_id)
);
CREATE TABLE local_identities (
  network_id TEXT NOT NULL,
  node_id TEXT NOT NULL,
  identity TEXT NOT NULL,
  PRIMARY KEY(network_id,node_id),
  FOREIGN KEY(network_id) REFERENCES networks(network_id)
);
CREATE TABLE events (
  network_id TEXT NOT NULL,
  hash TEXT NOT NULL,
  scope_id TEXT NOT NULL,
  author TEXT NOT NULL,
  sequence INTEGER NOT NULL,
  envelope TEXT NOT NULL,
  status TEXT NOT NULL,
  PRIMARY KEY(network_id,hash),
  FOREIGN KEY(network_id) REFERENCES networks(network_id)
);
CREATE INDEX events_author_sequence ON events(network_id,scope_id,author,sequence);
CREATE INDEX events_scope ON events(network_id,scope_id);
CREATE TABLE blobs (
  network_id TEXT NOT NULL,
  hash TEXT NOT NULL,
  size INTEGER NOT NULL,
  content BLOB NOT NULL,
  PRIMARY KEY(network_id,hash),
  FOREIGN KEY(network_id) REFERENCES networks(network_id)
);
CREATE TABLE conversations (
  network_id TEXT NOT NULL,
  conversation_id TEXT NOT NULL,
  title TEXT NOT NULL,
  PRIMARY KEY(network_id,conversation_id)
);
CREATE TABLE messages (
  network_id TEXT NOT NULL,
  conversation_id TEXT NOT NULL,
  event_hash TEXT NOT NULL,
  message_id TEXT NOT NULL,
  role TEXT NOT NULL,
  text TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  ordinal INTEGER NOT NULL,
  PRIMARY KEY(network_id,event_hash)
);
CREATE TABLE tasks (
  network_id TEXT NOT NULL,
  conversation_id TEXT NOT NULL,
  task_id TEXT NOT NULL,
  title TEXT NOT NULL,
  goal TEXT NOT NULL,
  acceptance TEXT NOT NULL,
  status TEXT NOT NULL,
  detail TEXT NOT NULL,
  event_hash TEXT NOT NULL,
  PRIMARY KEY(network_id,conversation_id,task_id)
);
CREATE TABLE execution_sessions (
  network_id TEXT NOT NULL,
  target_node TEXT NOT NULL,
  session_id TEXT NOT NULL,
  revision INTEGER NOT NULL,
  record TEXT NOT NULL,
  PRIMARY KEY(network_id,target_node,session_id)
);
CREATE TABLE operations (
  network_id TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  target_node TEXT NOT NULL,
  session_id TEXT,
  request TEXT NOT NULL,
  state TEXT NOT NULL,
  control INTEGER NOT NULL DEFAULT 0,
  result TEXT NOT NULL,
  PRIMARY KEY(network_id,operation_id)
);
CREATE INDEX operations_session ON operations(network_id,target_node,session_id,state);
CREATE TABLE resource_reservations (
  network_id TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  target_node TEXT NOT NULL,
  resource TEXT NOT NULL,
  PRIMARY KEY(network_id,operation_id,resource),
  FOREIGN KEY(network_id,operation_id) REFERENCES operations(network_id,operation_id)
);
CREATE TABLE continuation_claims (
  network_id TEXT NOT NULL,
  result_event_hash TEXT NOT NULL,
  policy_version TEXT NOT NULL,
  record TEXT NOT NULL,
  PRIMARY KEY(network_id,result_event_hash,policy_version)
);
CREATE TABLE notification_outbox (
  network_id TEXT NOT NULL,
  notification_id TEXT NOT NULL,
  record TEXT NOT NULL,
  derived INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(network_id,notification_id)
);
CREATE TABLE notification_acks (
  network_id TEXT NOT NULL,
  notification_id TEXT NOT NULL,
  client_id TEXT NOT NULL,
  event_hash TEXT NOT NULL DEFAULT '',
  PRIMARY KEY(network_id,notification_id,client_id)
);
CREATE TABLE budget_days (
  network_id TEXT NOT NULL,
  day TEXT NOT NULL,
  used INTEGER NOT NULL,
  PRIMARY KEY(network_id,day)
);
PRAGMA user_version = 3;
CREATE TABLE local_values (
  network_id TEXT NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  PRIMARY KEY(network_id,key)
);
