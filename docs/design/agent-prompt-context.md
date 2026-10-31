# Agent prompt and context contract

Vibeface compiles each Bot turn from a stable instruction object and a dynamic context object. The compiler accepts a validated snapshot and performs no Store I/O. Store reads the DAG basis, Bot profile, workspace, skills, memory records, work state, questions, sessions and provider settings. The Provider adapter chooses the wire representation.

## Stable instructions

Stable instructions contain the Bot profile, standing rules, workspace scope, Provider descriptor, tool catalog, skill index, security rules and a PromptFingerprint. The fingerprint includes Bot revision, workspace revision, Provider revision, skill revision, tool revision and scheduler permission version. The digest uses canonical JSON and excludes current time, message text, progress, native IDs, tokens and credential values.

The rendered profile is tag-safe. User text cannot close the profile block. Dynamic data is marked as data and cannot replace standing rules.

A native session is reusable only when its stable fingerprint, owner epoch, tool environment and permission fence remain valid. The model may be supplied per turn when the Provider does not bind it to the native session.

## Dynamic context

Dynamic context contains the VersionVector basis, trigger, bounded history, checkpoint, handoff, Memory delta, work and dispatch state, questions, pending checks, attachments, omitted counts and a retrieval route. Its fingerprint is separate from the stable fingerprint. The event DAG and deterministic checkpoint remain the shared source of truth.

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

The current implementation keeps the existing tool names and schemas while the descriptor supplies the stable catalog revision. A later tool discovery change can add search/describe/call without changing the execution owner.

## Data format

The Store uses data_epoch 9 for the memory metadata columns. It opens only the matching database epoch and does not migrate an older data directory. The migration hash is recorded in crates/node/src/config.rs. Protocol clients must treat another epoch as an incompatible data format.

This contract does not add a shared device pool, shared Provider account, remote screen control, phone projection or a second history store.
