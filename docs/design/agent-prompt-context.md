# Agent prompt and context contract

Vibeface compiles each Bot turn from a stable instruction object and a dynamic context object. The compiler accepts a validated snapshot and performs no Store I/O. Store reads the DAG basis, Bot profile, workspace, skills, memory records, work state, questions, sessions and provider settings. The Provider adapter chooses the wire representation.

## Stable instructions

Stable instructions contain the Bot profile, standing rules, workspace scope, Provider descriptor, tool catalog, skill index, security rules and a PromptFingerprint. The fingerprint includes the actual profile and standing-rule digests, Bot revision, workspace and allowed roots, Provider and model revision, skill body revisions, the full tool registry revision and Effect permission-policy version. Skill bodies stay outside the stable prompt; the bounded index carries their hashes. The digest uses canonical JSON and excludes current time, message text, progress, native IDs, tokens and credential values.

The rendered profile is tag-safe. User text cannot close the profile block. Dynamic data is marked as data and cannot replace standing rules.

A native session is reusable only when its stable fingerprint, owner epoch, tool environment and permission policy remain valid. Each attempt still has a separate permission fence that is revoked when its tools close. That per-attempt counter is not part of the stable fingerprint; otherwise every completed turn would invalidate the cache. The Provider may receive its model per turn. A changed model still changes the compiled fingerprint and opens a new generation.

## Dynamic context

Dynamic context contains the VersionVector basis, trigger, bounded history, checkpoint, handoff, bounded essential Memory snapshot, separate work and dispatch sections, questions, pending checks, attachments, omitted counts and a retrieval route. Each section records its own budget and omitted count. Its fingerprint is separate from the stable fingerprint. The event DAG and deterministic checkpoint remain the shared source of truth. Revision-based Thread Memory deltas remain required implementation work; the current snapshot must not be described as a delta.

Handoff is a bounded recovery record. It carries the source fingerprint, generation, handoff revision, DAG basis, open work and decisions. It is recovery data, not a system instruction. Older content remains available through the context/history tools.

## Session generation

A local SchedulerSession stores its generation and a bounded lineage in the serialized shown state. When the stable configuration or owner epoch changes, claim_next clears the native ID, native token, covers and shown data, increments the generation and records the old fingerprint and handoff revision. The active turn keeps its old generation. The next turn starts with the new one.

Provider session IDs, transcripts, tokens, environment values and credentials never enter shared events. Only bounded, redacted handoff information can cross a Node boundary.

## Memory

Memory rows carry epoch, revision, origin, user_controlled, essential and source event hash. Revision is the event rank projected by the reducer. Old event payloads default to the current epoch and Agent origin when replayed.

remember and forget write an attempt-scoped staged overlay. Each mutation records the base epoch and base revision. A completed turn commits the mutation with the reply and job terminal state in one Store transaction. Failed, interrupted and unknown turns discard it. A changed revision, epoch or user-controlled value leaves the newer record intact and records a local conflict.

Essential memory has an 8192-byte budget. The compiler reports omitted rows and keeps the omitted records available to recall. Recall returns the committed records plus the current turn's staged overlay.

## Provider and tools

ProviderDescriptor states session type, prompt mode, tool mode, skill injection, sandbox, steer, concurrency, compaction, usage, approval and model catalog support. It is a capability description, not a permission grant. The Node router remains the only owner of Effect checks, scoped tokens and call_ref idempotency.

The initial catalog contains tool_search, tool_describe, tool_call, context, read_skill and call_lookup. Search and describe expose the original domain schemas allowed in the current turn. tool_call resolves its target before the router checks Effect, invocation and lease. It rejects nested discovery and private internal tools. Direct scoped CLI calls retain the original handlers and call_ref semantics. The stable fingerprint covers the full registry, including tools not initially listed.

## Local diagnostic projection

The local shown state records a ContextProjection containing only fingerprints, generation, counts, byte sizes, Provider identity and observed lifecycle stage. Compilation records compiled; an actual Provider turn callback records started. Completion records completed, failed, cancelled, interrupted or unknown from the real outcome. A claimed job alone does not prove submission. Late callbacks cannot replace a terminal observation with started.

Conversation snapshots, records and owner RPC expose that bounded projection. Shared Web/Mobile parsing preserves it without native IDs, tokens, prompt text or Memory contents. An unreachable owner has unavailable context; local observations cannot replace the remote owner's observation. Queue metadata distinguishes main, task, schedule_routine and bot_message, but current conversation sessions remain serial. Only active_turn and steering_settlement currently describe real queue gates.

The essential Memory budget covers the entire serialized block. A data-only Memory review accepts at most 20 add/update/forget operations, rejects tools and secrets, and is disabled by default. Its proposals use the same staging and revision checks. Completion records conflict identifiers and revision metadata locally without copying candidate text into the diagnostic projection.

## Data format

The Store uses data_epoch 9 for the memory metadata columns. It opens only the matching database epoch and does not migrate an older data directory. The migration hash is recorded in crates/node/src/config.rs. Protocol clients must treat another epoch as an incompatible data format.

This contract does not add a shared device pool, shared Provider account, remote screen control, phone projection or a second history store.
