# Vibeface Web product

<!-- impeccable:product-schema 1 -->

Vibeface gives one person a workspace for directing Bot work, reviewing outcomes, and managing their own connected devices. Accounts, devices, events, attachments, routes and commands stay within that person's network.

The Web app supports Bot conversations, task conversations, channels, decisions, device joining, recovery, AI configuration, notifications, diagnostics and file-transfer experiments. Shared product state and operations come from `@vibeface/client`. The UI must preserve their identities, revisions and delivery outcomes.

The three narrow-screen tabs are messages, tasks and settings. A wide-screen sidebar lists real channels and Bots, with task attention and connection status. Route and back-navigation behavior remains part of the product contract.

Conversation sends retain their draft until the recorded outcome allows it to clear. Attachment download verifies the referenced size and SHA-256. A lost write response is an unknown result; the person must reconcile it before retrying. Reconnecting or refreshing history must not replay a side effect. Task stop, approval, retry and continuation actions use the existing operations and their original identifiers.

Devices belong to the person, with explicit management rights. Removing a device, withdrawing a manager or leaving a browser retains its consequence text and confirmation. The only manager cannot leave before another management device is available. Joining verifies the safety code. Recovery retains the original password, file, lost-device and waiting-state rules.

[Arc](https://github.com/WheatXie/uiarc-pro-reverse), pinned at `859606819c68787b2dccb54adcb9b08bd3272d5d`, is the only interface system. Product compositions adapt its sidebar, conversation, agent, settings and security items to real state. They do not introduce demo activity. See [DESIGN.md](DESIGN.md) and [MIGRATION.md](MIGRATION.md) for the source mapping and verification boundaries.

The experience must account for loading, empty results, offline records, waiting decisions, unknown outcomes, long text, reduced motion and keyboard use. Product copy names the consequence and next action. UI implementation details do not belong in the person's workflow.
