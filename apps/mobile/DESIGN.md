# Native Arc design

The native app ports [WheatXie/uiarc-pro-reverse](https://github.com/WheatXie/uiarc-pro-reverse) at `859606819c68787b2dccb54adcb9b08bd3272d5d`. Port ownership and upstream item names are recorded in [source.json](src/components/arc/source.json). Product callbacks, copy, routes and live state remain in the app and feature modules.

## Foundation

`global.css` imports the generated `@vibeface/theme/native.css`. Native code reads `arcTokens` and `arcColors` from the same generated package. Use the neutral Arc accent, semantic status colors, and automatic light/dark variants. Do not add local palettes.

Inter is the body and control face. Body text is 16px, metadata and controls are 14px, and small headings use 18px or 22px. Geist is reserved for native large navigation titles. Weights are 400 and 500. Geist Mono is available for IDs, commands and technical records. Native text retains Dynamic Type.

Controls and alerts have 18px corners. The pinned card uses 26px corners and 20px content padding; approval panels use their source 24px corners and 20px padding. Alerts and approval panels keep neutral surfaces and borders, with status color on the icon. Spacing follows Arc's 4px grid. Primary touch controls have a minimum height of 44px and grow with text. Small Arc buttons retain 36px visuals inside a padded 44px native hit container, with at least 4px of hitSlop on every edge. Large buttons are 50px. Selection groups use a 44px touch target even when the visual marker is smaller.

## Native ports and product regions

| Region | Arc items | Native composition |
| --- | --- | --- |
| Actions and fields | button, input, textarea | Four button variants: primary, secondary, ghost, danger. Border-only field focus, inline errors and existing loading states. |
| Selection | switch, radio-group, checkbox, segmented-control | Native accessibility roles, fixed hit targets, selection color and a moving segmented highlight. |
| Navigation and home | tabs, sidebar | Three existing root tabs: 消息, 任务, 设置. Wide windows use a muted rail and real route links. Channels remain in their existing entry paths. |
| Chat | ai-chat, ai-composer, chat-thread | Neutral message bubbles, full-width assistant replies, one bordered multiline composer and attachment chips. |
| Tasks and decisions | agent-run, agent-approvals, metric-card, alert | Task counts and filters, status rows, approval panels and inline accept/reject/retry controls. 待处理 remains at the top of the task screen. |
| Settings and devices | settings-page, security-settings, card | Section headings, divided settings rows, plain icons and native navigation. |
| Joining and recovery | stepper, input, alert, button | Existing scan/paste/recovery stages, safety-code checks, cancellation and confirmation. |
| Transfers and logs | card, agent-run, progress | Real transfer records, budgets, deadlines, audit details, native file operations and existing retry controls. |
| Feedback | skeleton, alert, toast | Finite skeleton pulse, persistent foreground errors and a separately announced background queue. |

There are no React Native Reusables components, CVA, slot wrappers or WebViews. Custom action glyph tiles and decorative glows are removed. Bot faces preserve identity, shape, chosen color and status data, with solid fills. Their motion follows the existing shared avatar engine and stops for reduced motion and covered screens.

## Platform behavior

Expo Router stacks own native headers, modal presentation, Back and edge gestures. Safe-area padding, keyboard-aware scrolling, sticky composer placement, selection retention, private attachment lifecycle and native camera/file/share/paste controls remain platform owned. Chat panels use an opaque Arc surface on iOS and Android.

Arc timing and easing come from the generated tokens. Control presses, entry, exit and layout changes respect reduced motion. Native screen-reader roles, labels, selected/checked/expanded states and announcements remain attached to the real controls. Color never substitutes for a status label.

Unknown write outcomes remain unknown. A visual change cannot create a success state, retry a write automatically, bypass reconciliation or unlock a pending submission.

## Verification

Run `pnpm --filter vibeface-mobile typecheck`, `pnpm --filter vibeface-mobile test:unit`, `python3 scripts/check-uiarc.py`, and the lightweight Expo production export during integration. Type checks, source audits and exports do not prove native device behavior or visual parity. This task does not run emulators, screenshots or UI acceptance tests.
