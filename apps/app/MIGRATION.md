# Web Arc migration

Authority: [WheatXie/uiarc-pro-reverse](https://github.com/WheatXie/uiarc-pro-reverse), commit `859606819c68787b2dccb54adcb9b08bd3272d5d`. Exact controls live in `packages/uiarc`; adapted product compositions are recorded in [source.json](src/components/arc/source.json).

| Product region | Arc source | Integration |
| --- | --- | --- |
| Actions and navigation actions | button | Direct Arc variants and sizes; real anchors use the same CSS Module and retain href behavior |
| Submitted fields | input, textarea, select, checkbox | Real names, values, required state, handlers and validation; select keys round trip through a private prefix, including empty values |
| Immediate settings | switch, segmented-control | Real settings callbacks; no simulated saving |
| Desktop navigation and mobile drawer | sidebar, drawer | Real routes, Bots, channels, unread and waiting counts, connection, new Bot, messages/tasks/settings tabs |
| Messages and scroll history | ai-chat, chat-thread | Real transcript, streaming, delivery states, history anchors, unread and latest navigation |
| Composer | ai-composer, textarea, button, badge | Retained draft, attachment limits, Enter/Shift+Enter, stop and send callbacks |
| Work and task detail | agent-run, drawer, badge | Real state, revision, stop/continue, stale records, unknown outcome reconciliation and independent scrolling |
| Decisions | agent-approvals, alert, button | Existing allow, reject, answer, ignore, retry and confirmation actions |
| Devices and settings | settings-page, security-settings | Real directory, AI providers, folders, permissions, diagnostics, notifications and consequences |
| Joining and recovery | stepper, input, button, alert | Real invitation, safety code, scan video, passwords, recovery files and explicit decisions |
| Feedback | empty-state, skeleton, alert, badge | Real loading, empty, offline, disabled and failure states with nearby actions |

The prior components/ui implementations, component registry configuration, CVA, shadcn, the umbrella Radix package and the previous animation stylesheet were removed. Arc's own scoped Radix dependencies remain inside its pinned controls.

The task overview now uses a compact status summary and record grouping. Its old promotional heading, decorative glow and nested metric tiles were removed. Bot face artwork remains product data; it follows Arc's theme attribute. Controls no longer take a Bot-specific palette.

Behavior verification uses existing product tests and the TypeScript compiler. The production build requires the real secure WebAssembly output built from the current source. Browser acceptance, screenshots and e2e are excluded unless the user explicitly requests them. CSS source inspection covers width constraints, min-width, wrapping, safe areas and reduced motion; it does not establish visual acceptance.
