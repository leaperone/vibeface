# Agent prompt and context contract

Vibeface builds each Bot turn from two inputs. Stable instructions describe the Bot identity, standing rules, workspace boundary, provider capabilities, skills, and tool discovery. Dynamic context describes the DAG basis, the trigger, recent messages, checkpoints, work, questions, attachments, and memory changes for that turn.

The stable input receives a fingerprint. A Bot, workspace, provider, model, skill, tool, or permission change creates a new fingerprint and prevents a native session from carrying the old instructions into a later turn. A message, work update, checkpoint, or current time changes dynamic context only.

The event DAG remains the shared source of truth. Native provider sessions, session tokens, provider transcripts, environment values, and credentials remain local to the Node. A handoff records bounded references to the DAG and recovery information. It never becomes a system instruction.

Memory mutations are scoped to the running turn. The compiler exposes a staged overlay to that turn, but the Store commits it only with a completed reply and a matching revision. Failed, interrupted, or unknown turns discard staged mutations. A revision conflict leaves the newer value unchanged.

Provider descriptors state the prompt mode, session type, tool mode, skill injection, concurrency, compaction, approval, and model catalog capabilities. Descriptors select the adapter path. They do not grant permissions. The Node router remains the owner of tool validation, effect checks, capability tokens, and idempotent `call_ref` handling.

This contract does not add a shared device pool, a shared provider account, remote screen control, or a second history store. Those features require separate product and security decisions.
