# Vibeface Web design

The Web interface uses [Arc](https://github.com/WheatXie/uiarc-pro-reverse) at commit `859606819c68787b2dccb54adcb9b08bd3272d5d`. The pinned components and their hashes live in `packages/uiarc`. `packages/theme/theme.css` imports Arc foundation and exposes its semantic tokens. Do not add another component library or palette.

Arc controls are imported directly from `@vibeface/uiarc/<item>/<item>.tsx`. Product compositions live in `src/components/arc`. Their [source map](src/components/arc/source.json) records the upstream items they adapt. They bind actual route, conversation, device, recovery and task data. Demo people, model catalogs, messages and simulated progress must not enter product state.

The interface uses Inter for body text and controls, Geist for display headings, and Geist Mono for code. Sizes come from the Arc scale, with 16px body text and 14px secondary text. Weights are 400 and 500. Controls use Arc's 18px radius; grouped panels use 26px and standalone surfaces use 34px. Components own their appearance through their pinned CSS Modules. App CSS owns arrangement, safe areas and product content.

Theme selection writes `data-theme="light"` or `data-theme="dark"` on `html`; the neutral accent is the default. Colors use background, surface, surface-raised, surface-muted, foreground, text-secondary, text-muted, border and status tokens. Accent identifies selected state or progress. Status color accompanies a label or an accessible icon. Arc foundation supplies keyboard focus treatment.

The desktop shell has an Arc sidebar rail with real channels, Bots, task attention and connection state. Narrow screens use an Arc drawer and preserve the existing messages, tasks and settings tabs. One workspace page container supplies the gutters. Conversation and task inspectors retain independent scrolling and readable long names.

Conversation presentation derives from ai-chat, chat-thread and ai-composer. The composer keeps attachment selection and validation, Enter submission, Shift+Enter, stop, retained drafts and disabled explanations. A thread follows new content while at its end, preserves the visible history anchor while reading, and provides unread and latest-message navigation. Delivery uncertainty remains explicit.

Task views use agent-run record grouping and agent-approvals decision structure. Task counts are actual records, never simulated progress. Unknown outcomes require reconciliation before another operation. Device and security forms use settings-page and security-settings grouping, with Arc fields, switches, checkboxes, select controls and drawers. Joining and recovery use Arc stepper, alerts and fields while retaining security-code verification and recovery consequences.

Feedback has explicit loading, empty, error, offline, waiting, disabled and success states. Inline errors stay beside their action. Background notices preserve their action link and dismissal. Long identifiers can expand; filenames preserve their extension. Reduced motion and native safe-area handling apply throughout.

Run `python3 scripts/check-uiarc.py`, the Web TypeScript check, existing behavior tests and the production build for changes to this system. Project policy reserves browser acceptance, screenshots and e2e runs for an explicit user request. Compilation does not establish visual acceptance.
