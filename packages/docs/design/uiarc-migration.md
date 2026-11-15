# Arc UI migration

The Web workspace and native mobile app use the Arc design system from
[WheatXie/uiarc-pro-reverse](https://github.com/WheatXie/uiarc-pro-reverse), pinned to
`859606819c68787b2dccb54adcb9b08bd3272d5d`.

## Completion requirements

- Every existing Web and Mobile route keeps its product operations, copy, data and recovery behavior.
- Web controls use Arc source components and Arc product compositions. Remove the old shadcn components, configuration and dependencies.
- Mobile controls are native ports of named Arc items. Remove the React Native Reusables implementations and configuration. Do not use a WebView.
- Both apps derive colors, typography, radii, control sizes and motion from the pinned Arc foundation. Do not maintain another hand-written palette.
- Rewrite the navigation, chat, tasks, settings, devices, joining and recovery compositions in the same system. Updating imports or a theme alone does not finish this migration.
- A committed guard rejects old UI imports, dependencies and theme roles, and verifies vendored source and generated token integrity.
- Run the relevant behavior tests, type checks and production builds on the final source. Keep the complete Web output and the mobile build evidence reviewable.
- Open one new PR. Merge and release require separate user authorization.

## Implementation boundaries

`packages/uiarc/src` holds unmodified, pinned Web component sources. `source.json` records upstream paths and SHA-256 hashes. Acquire further items with `python3 scripts/vendor-uiarc.py /path/to/pinned/upstream item ...`. Product data, routing and callback integration belong to each app, outside vendored source.

`packages/theme` derives the Web and native token files from Arc's `foundation.css` and motion tokens. Web uses Arc's `data-theme` attribute. Mobile resolves the same light and dark values to native-compatible colors. Layout utilities are allowed; they must not define a second component style or palette.

Native ports live under `apps/mobile/src/components/arc`. Each port names its upstream item and preserves Arc's semantic tokens, state model, control sizing and motion. Keep platform navigation, safe areas, Dynamic Type, keyboard behavior, Back and accessibility announcements.

Product components compose Arc controls with real Vibeface state. Arc's showcase blocks contain demonstration people, model lists and simulated work. Those fixtures must not enter product flows; extract their layout and visual rules into data-driven compositions instead.

## Work plan

1. Record the route, component, action and import census. Pin and acquire the source.
2. Replace the shared foundation and generate native tokens. Establish migration checks.
3. Rewrite Web controls, navigation and all route compositions. Remove the old Web layer.
4. Rewrite Mobile controls, navigation and all route compositions. Remove the old native layer.
5. Integrate both apps, run behavior and build checks, and review remaining migration gaps.
6. Update durable instructions and design records, then deliver one PR and build artifacts.

The Web and Mobile implementation workstreams own separate app directories. The root owns shared packages, source provenance, the lockfile, policy, verification and integration.

## Region mapping

| Product region | Arc sources | Integration requirements |
| --- | --- | --- |
| Actions and fields | button, input, textarea, select, checkbox, switch, segmented-control | Preserve labels, validation, loading, disabled states and submitted values |
| Navigation | sidebar, tabs, drawer | Real routes, unread counts, connection state, responsive navigation |
| Conversation | ai-chat, ai-composer, chat-thread | Real messages, streaming, attachments, cancel, approvals and preserved scroll |
| Work and decisions | agent-run, agent-approvals, badge, progress | Real task state, accept/reject, stop, retry and unknown-result reconciliation |
| Settings and devices | settings and security blocks, card, dialog | Preserve device identities, AI configuration, permissions and destructive consequences |
| Joining and recovery | stepper, input, button, alert | Preserve scanning, paste, network choice, safe-code checks and recovery |
| Feedback | empty-state, skeleton, alert, toast | Product state, accessible labels and inline errors |

## Verification constraints

Project instructions reserve e2e tests, screenshots and UI acceptance runs for an explicit request. This migration uses existing behavior tests, targeted component checks, source audits and production builds. Do not claim visual or native-device verification from compilation alone.

The baseline census is 17 Web route files and 27 native route files, including layouts. The initial dependency census includes 339 internal component imports. Counts may change as components are reorganized; product routes and actions remain the acceptance scope.
