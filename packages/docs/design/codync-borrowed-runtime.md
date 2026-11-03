# Codync borrowed runtime boundaries

This design borrows runtime mechanisms from Codync without changing Vibeface's signed event log or personal network model.

## Delivery ordering

All requests sent to one target Node use one per-device gate. Message delivery, work dispatch, and stop requests therefore keep one order for that target. Different target Nodes may still run in parallel.

The gate is local process state. The trigger, owner, and idempotency record remain in the Store. A lost reply does not create a second trigger.

## Mailbox state

The scheduler outbox remains the source of active work. A bounded local mailbox record adds the delivery state, attempt count, next attempt time, creation time, and scrubbed last error. The record uses the existing private metadata store, so this change does not add a database migration.

The mailbox keeps active entries until delivery, reconciliation, or an explicit retry limit reaches a terminal state. It rejects new entries once the per-network bound is full, so an old active trigger is never evicted silently. Terminal records remain for a retention window. Successful delivery becomes the delivered state and removes the scheduler outbox in one transaction; startup also repairs a delivered record left beside an older outbox row.

The unknown state means that the target may have accepted the request. It does not mean that the business action failed. The owner still admits a trigger idempotently, and Provider turns keep their existing read-back and pending-check rules.

## Client projection recovery

The Node assigns a local revision to each projection update. A client receives the revision with a delta. A broadcast lag or a revision gap produces a resync message, and the client reads a fresh snapshot.

The revision is not a causal clock. Signed events, VersionVector, fork evidence, and authorization projection remain the source of truth.

## Provider boundary

ProviderDescriptor and FailureKind remain the business boundary. Provider-specific runtimes keep their native protocols, session files, and read-back rules. Local adapter evidence may include a bounded, scrubbed stderr tail and process termination facts. These facts never enter events, peer outbox payloads, or shared workspace records.

The Provider evidence helper exposes the bounded fields a local workaround ledger can record. This PR does not write a ledger entry until a concrete workaround is observed. A future ledger remains diagnostic data and cannot select a backup provider or replay a turn.

## Out of scope

This change does not add HTML artifacts, group chat, reactions, device test isolation, a central account, a cloud relay, default multi-network support, or automatic retry after an uncertain Provider mutation.
