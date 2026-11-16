# Native Arc migration

The mobile UI uses native ports of the fixed Arc source. The item mapping and platform adaptations are in `src/components/arc/source.json`. Colors, sizes and motion come from generated `@vibeface/theme` tokens. The old Reusables components, configuration and dependencies are removed.

The migration preserves the three root destinations, 消息、任务、设置. Pending decisions remain inside the task page. Native navigation, Back, attachments, camera, sharing, keyboard handling, safe areas and Dynamic Type keep their existing platform ownership.

The source corrections from independent review include neutral alert and approval panels, Card radius 26px with 20px padding, per-size Arc line heights, and 44px interactive containers around 36px small buttons. Severity appears in the status glyph and copy.

Run `node scripts/check-uiarc-native.cjs` from the repository root to check the actual iOS and Android styles produced by Uniwind. Run `node scripts/check-uiarc-parity.cjs` to compare the original product copy and event expressions against the migration baseline. The recorded baseline has 1,039 Chinese copy instances and 288 event bindings across 51 product modules.

These checks supplement the mobile type check, unit suite and Expo production export. They do not replace installed-app, screen-reader, keyboard, gesture or visual acceptance on a device. This migration did not run device or screenshot acceptance.
