# Codync borrowed runtime boundaries

This design borrows runtime mechanisms from Codync without changing Vibeface's signed event log or personal network model.

## Delivery ordering

Scheduler delivery requests sent to one target Node use one per-device gate. Message delivery, work dispatch, and stop requests therefore keep one order for that target. Other peer RPCs keep their own idempotency and version checks. Different target Nodes may still run in parallel.

The gate is local process state. The trigger, owner, and idempotency record remain in the Store. A lost reply does not create a second trigger.

## Mailbox state

The scheduler outbox remains the source of active work. A bounded local mailbox record adds the delivery state, attempt count, next attempt time, creation time, and scrubbed last error. The record uses the existing private metadata store, so this change does not add a database migration.

The mailbox keeps active entries until delivery. Attempt counts and backoff are bounded, but repeated delivery failures keep the scheduler outbox for a later retry instead of silently dropping an undelivered trigger. A later delivery pass reevaluates the current owner, so handoff can release the pressure. It rejects new entries once the per-network bound is full, so an old active trigger is never evicted silently. Terminal records remain for a retention window. Successful delivery becomes the delivered state and removes the scheduler outbox in one transaction; startup also repairs a delivered record left beside an older outbox row.

The unknown state means that the target may have accepted the request. It does not mean that the business action failed. The owner still admits a trigger idempotently, and Provider turns keep their existing read-back and pending-check rules.

## Client projection recovery

When a push burst or a broadcast lag can leave a client behind, the Node sends a `resync` message and the client reads a fresh snapshot. Progress updates also carry their own sequence, and a sequence gap schedules a refresh for that progress stream. This protocol does not attach a unified revision to every projection delta.

The resync signal and progress sequence are not causal clocks. Signed events, VersionVector, fork evidence, and authorization projection remain the source of truth.

## Provider boundary

ProviderDescriptor and FailureKind remain the business boundary. Provider-specific runtimes keep their native protocols, session files, and read-back rules. Local adapter evidence may include a bounded, scrubbed stderr tail and process termination facts. These facts never enter events, peer outbox payloads, or shared workspace records.

The Provider evidence helper exposes the bounded fields a local workaround ledger can record. This PR does not write a ledger entry until a concrete workaround is observed. A future ledger remains diagnostic data and cannot select a backup provider or replay a turn.

## Out of scope

This change does not add HTML artifacts, group chat, reactions, device test isolation, a central account, a cloud relay, default multi-network support, or automatic retry after an uncertain Provider mutation.
