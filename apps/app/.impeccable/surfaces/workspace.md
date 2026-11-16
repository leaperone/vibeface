# Web workspace

Mode: operate. The person reads the current Bot, task or channel, reviews states that need attention, and performs the next action.

Arc at `859606819c68787b2dccb54adcb9b08bd3272d5d` is the only UI authority. `src/components/arc/source.json` maps product compositions to pinned items. `packages/theme/theme.css` derives semantic colors, typography, radii and motion from foundation. The theme attribute is `data-theme`; Inter is the body face, Geist the display face and Geist Mono the code face.

The desktop sidebar rail lists real channels and Bots. Narrow screens retain messages, tasks and settings tabs and use an Arc navigation drawer. Conversation and task inspectors have separate scroll areas. Task overview uses real counts and record grouping, with no decorative hero. Settings and devices group related rows and actual controls. Joining and recovery preserve code checks and consequences.

Preserve product copy, route ownership, keyboard actions, async status, retained drafts, attachment validation and verified download. Preserve unknown results until reconciliation. Do not replace operations with showcase activity or computed fake progress. Every region must represent loading, empty, offline, waiting, recovery, failure, disabled state and long values.

Arc controls own their appearance. Product CSS Modules own layout and domain presentation. Arc foundation owns keyboard focus; all motion respects reduced-motion preferences. Native video capture and hidden attachment picking remain browser accommodations.

Verification uses source audit, TypeScript, existing behavior tests and a real production build. Project policy reserves screenshots, e2e and browser acceptance for an explicit user request; no visual acceptance is implied by source review or compilation.
