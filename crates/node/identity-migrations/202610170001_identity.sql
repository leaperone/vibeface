CREATE TABLE IF NOT EXISTS identity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS identity_network (network_id TEXT PRIMARY KEY, state TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS identity_requests (
    request_id TEXT PRIMARY KEY, subject_id TEXT NOT NULL, kind TEXT NOT NULL,
    response TEXT NOT NULL, created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS identity_invites (
    invite_id TEXT PRIMARY KEY, request_id TEXT NOT NULL UNIQUE, network_id TEXT NOT NULL,
    inviter_subject_id TEXT NOT NULL, payload TEXT NOT NULL, payload_hash TEXT NOT NULL,
    capability_hash TEXT NOT NULL, inviter_signature TEXT NOT NULL,
    epoch INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL, used INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS identity_pair_reviews (
    invite_id TEXT PRIMARY KEY, review TEXT NOT NULL, review_hash TEXT NOT NULL,
    inviter_signature TEXT NOT NULL, new_subject_signature TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS identity_pair_requests (
    request_id TEXT PRIMARY KEY, invite_id TEXT NOT NULL UNIQUE, subject_id TEXT NOT NULL,
    review TEXT NOT NULL, review_hash TEXT NOT NULL, new_subject_signature TEXT NOT NULL DEFAULT ''
);
PRAGMA user_version = 1;
